use super::{
    config::HaRuntimeConfig,
    election::{HaDecisionReason, HaState},
    runtime::HaLoopState,
    session::PeerRejections,
};
use crate::config::FailurePolicy;
use crate::gruezi::application::{ApplicationPhase, ApplicationState};
use crate::gruezi::hooks::{LifecycleKind, LifecycleOutcome};
use crate::gruezi::ownership::{AuthoritySource, FencingState, OwnershipController, ResourceState};
use crate::gruezi::watchdog::{WatchdogManager, WatchdogStatus};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceStatus {
    pub state: String,
    pub confirmed: bool,
    pub observed_ago_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityStatus {
    pub state: String,
    pub source: Option<AuthoritySource>,
    pub term: u64,
    /// Conservative remaining lifetime of the current grant, in milliseconds
    /// (`None` for `assumed` authority, which only ends when the elected role
    /// or the policy says so).
    pub expires_in_ms: Option<u64>,
    /// Grant cutoff remains visible while acquisition waits for fencing.
    #[serde(default)]
    pub grant_expires_in_ms: Option<u64>,
    #[serde(default)]
    pub fencing: FencingState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyLevel {
    Safe,
    Uncertain,
    Assumed,
}

impl SafetyLevel {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Uncertain => "uncertain",
            Self::Assumed => "assumed",
        }
    }
}

/// Protocol-layer view of the peer (design spec section 6: `peer`). Named
/// `peer_session` because the existing `peer` field already carries the peer
/// address and existing fields keep their meaning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSessionStatus {
    /// An authenticated, non-replayed datagram arrived within the follower
    /// deadline (fresh or not).
    pub reachable: bool,
    /// The current peer session echoed our nonce and its newest fresh advert
    /// is within the follower deadline. Becomes false as soon as the peer
    /// restarts with a new nonce.
    pub session_established: bool,
    pub rejections: PeerRejections,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyStatus {
    pub level: SafetyLevel,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleOperationStatus {
    pub event: String,
    pub generation: u64,
    pub remaining_deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriticalFailureStatus {
    pub event: String,
    pub generation: u64,
    pub outcome: String,
    pub elapsed_ms: u64,
    pub exit_code_or_signal: Option<i32>,
    pub stderr_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationStatus {
    pub state: String,
    pub last_verified_ago_ms: Option<u64>,
    pub next_verify_in_ms: Option<u64>,
    pub db_incarnation: Option<String>,
}

/// Completed-operation results plus the independent verification freshness fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleConfirmations {
    pub activation_confirmed: bool,
    pub passive_confirmed: bool,
    pub verified_fresh: bool,
}

/// Completed operations and ongoing verification are separate evidence.
/// Fresh verification proves bounded liveness, never release finality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleStatus {
    pub configured: bool,
    pub phase: String,
    pub generation: u64,
    pub operation: Option<LifecycleOperationStatus>,
    /// Flattened so the wire keys stay flat while each struct keeps few bools.
    #[serde(flatten)]
    pub confirmations: LifecycleConfirmations,
    pub verification: VerificationStatus,
    pub last_critical_failure: Option<CriticalFailureStatus>,
}

const fn lifecycle_event_name(kind: LifecycleKind) -> &'static str {
    match kind {
        LifecycleKind::Activate => "activate",
        LifecycleKind::Deactivate => "deactivate",
        LifecycleKind::Verify => "verify",
        LifecycleKind::Notify(event) => event.as_env_value(),
    }
}

fn sanitized_stderr_tail(stderr: &str) -> String {
    let mut size = 0;
    let tail: Vec<_> = stderr
        .chars()
        .rev()
        .filter(|c| !c.is_control())
        .take_while(|c| {
            size += c.len_utf8();
            size <= 1_024
        })
        .collect();
    tail.into_iter().rev().collect()
}

impl VerificationStatus {
    fn snapshot(
        application: &ApplicationState,
        verified_fresh: bool,
        now: tokio::time::Instant,
    ) -> Self {
        let mut state = if application.configured() {
            "scheduled"
        } else {
            "not_configured"
        };
        let mut verification = Self {
            state: String::new(),
            last_verified_ago_ms: None,
            next_verify_in_ms: None,
            db_incarnation: None,
        };
        if let ApplicationPhase::Ready {
            verified_at,
            next_verify_at,
            db_incarnation,
            ..
        } = &application.phase
        {
            verification.last_verified_ago_ms = Some(duration_millis_u64(
                now.saturating_duration_since(*verified_at),
            ));
            verification.next_verify_in_ms = Some(duration_millis_u64(
                next_verify_at.saturating_duration_since(now),
            ));
            verification.db_incarnation.clone_from(db_incarnation);
            state = if !verified_fresh {
                "stale"
            } else if application
                .operation
                .as_ref()
                .is_some_and(|op| op.kind == LifecycleKind::Verify)
            {
                "verifying"
            } else if now >= *next_verify_at {
                "scheduled"
            } else {
                "fresh"
            };
        } else if application
            .last_critical_failure
            .as_ref()
            .is_some_and(|failure| failure.kind == LifecycleKind::Verify)
        {
            state = "failed";
        }
        state.clone_into(&mut verification.state);
        verification
    }
}

impl LifecycleStatus {
    fn snapshot(
        application: &ApplicationState,
        current_generation: u64,
        verification_deadline: Option<tokio::time::Instant>,
        now: tokio::time::Instant,
    ) -> Self {
        let (phase, generation) = match application.phase {
            ApplicationPhase::Disabled { generation } => ("not_configured", generation),
            ApplicationPhase::Inactive { generation } => ("inactive", generation),
            ApplicationPhase::Activating { generation } => ("activating", generation),
            ApplicationPhase::Ready { generation, .. } => ("ready", generation),
            ApplicationPhase::Deactivating { generation } => ("deactivating", generation),
            ApplicationPhase::Uncertain { generation } => ("uncertain", generation),
        };
        let configured = application.configured();
        let activation_confirmed = application.activation_confirmed(current_generation);
        let verified_fresh =
            activation_confirmed && verification_deadline.is_some_and(|deadline| now < deadline);
        Self {
            configured,
            phase: phase.to_owned(),
            generation,
            operation: application
                .operation
                .as_ref()
                .map(|operation| LifecycleOperationStatus {
                    event: lifecycle_event_name(operation.kind).to_owned(),
                    generation: operation.generation,
                    remaining_deadline_ms: duration_millis_u64(
                        operation.deadline.saturating_duration_since(now),
                    ),
                }),
            confirmations: LifecycleConfirmations {
                activation_confirmed: configured && activation_confirmed,
                passive_confirmed: configured && application.passive_confirmed(current_generation),
                verified_fresh,
            },
            verification: VerificationStatus::snapshot(application, verified_fresh, now),
            last_critical_failure: application.last_critical_failure.as_ref().map(|failure| {
                CriticalFailureStatus {
                    event: lifecycle_event_name(failure.kind).to_owned(),
                    generation: failure.generation,
                    outcome: match failure.outcome {
                        LifecycleOutcome::Succeeded => "succeeded",
                        LifecycleOutcome::Failed => "failed",
                        LifecycleOutcome::TimedOut => "timed_out",
                        LifecycleOutcome::SpawnError => "spawn_error",
                        LifecycleOutcome::Cancelled => "cancelled",
                    }
                    .to_owned(),
                    elapsed_ms: duration_millis_u64(failure.elapsed),
                    exit_code_or_signal: failure.exit_code_or_signal,
                    stderr_tail: sanitized_stderr_tail(&failure.stderr_tail),
                }
            }),
        }
    }
}

/// What drove the last recorded transition (design spec section 6): the
/// election decision, the policy layer (bilateral consent), shutdown or a
/// runtime fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionTrigger {
    Election,
    Consent,
    Shutdown,
    Fault,
    External,
}

impl TransitionTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Election => "election",
            Self::Consent => "consent",
            Self::Shutdown => "shutdown",
            Self::Fault => "fault",
            Self::External => "external",
        }
    }
}

/// `last_transition` snapshot (design spec section 6): why the last recorded
/// transition happened, what triggered it and how long ago.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastTransitionStatus {
    pub reason: HaDecisionReason,
    pub trigger: TransitionTrigger,
    pub ago_ms: u64,
}

/// Status wire name of the configured failure policy.
const fn failure_policy_name(policy: &FailurePolicy) -> &'static str {
    match policy {
        FailurePolicy::Strict => "strict",
        FailurePolicy::Available => "available",
        FailurePolicy::External => "external",
    }
}

fn ownership_status(
    ownership: &OwnershipController,
    now: Instant,
) -> (ResourceStatus, AuthorityStatus, SafetyStatus) {
    let resource = ownership.resource_state();
    let authority = ownership.authority();
    let (level, why) = if authority.source() == Some(AuthoritySource::Assumed) {
        // Exact spec-section-6 mapping: whenever the authority source is
        // `assumed` the safety level is `assumed`, owned or not.
        (
            SafetyLevel::Assumed,
            "ownership relies on local timeout authority",
        )
    } else if ownership.owned() || ownership.can_release_handoff() {
        (SafetyLevel::Safe, "resource and authority are confirmed")
    } else {
        (
            SafetyLevel::Uncertain,
            "resource or authority is not confirmed",
        )
    };
    (
        ResourceStatus {
            state: resource.as_str().to_owned(),
            confirmed: matches!(
                resource,
                ResourceState::Absent | ResourceState::Present { .. } | ResourceState::Unmanaged
            ),
            observed_ago_ms: ownership
                .last_observed_at()
                .map(|observed| saturating_elapsed_ms(now, observed.into_std())),
            last_error: ownership.last_error().map(str::to_owned),
        },
        AuthorityStatus {
            state: authority.as_str().to_owned(),
            source: authority.source(),
            term: authority.term(),
            expires_in_ms: authority.expiry().map(|expiry| {
                duration_millis_u64(expiry.into_std().saturating_duration_since(now))
            }),
            grant_expires_in_ms: ownership.external_grant_expiry().map(|expiry| {
                duration_millis_u64(expiry.into_std().saturating_duration_since(now))
            }),
            fencing: ownership.fencing(),
        },
        SafetyStatus {
            level,
            why: why.to_owned(),
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaStatus {
    pub node_id: String,
    pub group_id: String,
    pub bind: String,
    pub peer: String,
    pub state: HaState,
    pub owned: bool,
    pub resource: ResourceStatus,
    pub authority: AuthorityStatus,
    pub safety: SafetyStatus,
    pub priority: u8,
    pub advert_interval_ms: u64,
    pub dead_timeout_ms: u64,
    pub hold_down_ms: u64,
    pub sequence: u64,
    pub peer_node_id: Option<String>,
    pub peer_state: Option<HaState>,
    pub peer_alive: bool,
    pub last_peer_seen_ms_ago: Option<u64>,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub invalid_packets: u64,
    pub auth_failures: u64,
    pub duplicate_node_id_packets: u64,
    pub decision_reason: HaDecisionReason,
    pub last_transition_reason: Option<HaDecisionReason>,
    pub last_transition_ms_ago: Option<u64>,
    /// Structured transition record (reason, trigger, age) from the policy
    /// layer; additive to `last_transition_*`.
    pub last_transition: Option<LastTransitionStatus>,
    /// Configured failure policy (`strict | available | external`).
    pub failure_policy: String,
    pub peer_session: PeerSessionStatus,
    pub watchdog: WatchdogStatus,
    pub lifecycle: LifecycleStatus,
    /// Hex-encoded wire sender nonce, empty only before a runtime snapshot.
    pub instance_id: String,
}

impl HaStatus {
    #[must_use]
    pub fn new(
        node_id: impl Into<String>,
        group_id: impl Into<String>,
        bind: impl Into<String>,
        peer: impl Into<String>,
    ) -> Self {
        let (resource, authority, safety) = ownership_status(
            &OwnershipController::new(crate::gruezi::ownership::ResourceManagement::Managed),
            Instant::now(),
        );
        Self {
            node_id: node_id.into(),
            group_id: group_id.into(),
            bind: bind.into(),
            peer: peer.into(),
            state: HaState::Init,
            owned: false,
            resource,
            authority,
            safety,
            priority: 0,
            advert_interval_ms: 0,
            dead_timeout_ms: 0,
            hold_down_ms: 0,
            sequence: 0,
            peer_node_id: None,
            peer_state: None,
            peer_alive: false,
            last_peer_seen_ms_ago: None,
            packets_sent: 0,
            packets_received: 0,
            invalid_packets: 0,
            auth_failures: 0,
            duplicate_node_id_packets: 0,
            decision_reason: HaDecisionReason::Startup,
            last_transition_reason: None,
            last_transition_ms_ago: None,
            last_transition: None,
            failure_policy: String::new(),
            peer_session: PeerSessionStatus::default(),
            watchdog: WatchdogManager::default().status(tokio::time::Instant::now()),
            lifecycle: LifecycleStatus::snapshot(
                &ApplicationState::disabled(0),
                0,
                None,
                tokio::time::Instant::now(),
            ),
            instance_id: String::new(),
        }
    }

    fn apply_application_status(
        &mut self,
        application: &ApplicationState,
        current_generation: u64,
        verification_deadline: Option<tokio::time::Instant>,
        now: tokio::time::Instant,
    ) {
        self.lifecycle =
            LifecycleStatus::snapshot(application, current_generation, verification_deadline, now);
        if !self.lifecycle.configured {
            return;
        }
        let blocked = if matches!(
            application.phase,
            ApplicationPhase::Deactivating { .. } | ApplicationPhase::Uncertain { .. }
        ) {
            Some("lifecycle passive confirmation pending; ownership and release blocked")
        } else if !self.lifecycle.confirmations.activation_confirmed {
            Some("lifecycle activation is not confirmed; ownership blocked")
        } else if !self.lifecycle.confirmations.verified_fresh {
            Some("lifecycle verification is stale; ownership blocked")
        } else {
            None
        };
        if let Some(why) = blocked {
            self.owned = false;
            // Preserve assumed authority's meaning even when application evidence blocks it.
            if self.safety.level != SafetyLevel::Assumed {
                self.safety.level = SafetyLevel::Uncertain;
            }
            why.clone_into(&mut self.safety.why);
        }
    }
}

pub(super) fn publish_loop_status(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &HaLoopState,
    now: Instant,
) {
    publish_status(status_tx, status_snapshot(runtime, loop_state, now));
}

fn status_snapshot(runtime: &HaRuntimeConfig, loop_state: &HaLoopState, now: Instant) -> HaStatus {
    let (resource, authority, safety) = ownership_status(&loop_state.ownership, now);
    let live_peer = loop_state
        .peer_observation
        .as_ref()
        .filter(|peer| runtime.follower_deadline(peer.observed_at) > now);
    let last_peer_seen_ms_ago = loop_state
        .peer_observation
        .as_ref()
        .map(|peer| saturating_elapsed_ms(now, peer.observed_at));
    let within_deadline =
        |seen: Option<Instant>| seen.is_some_and(|seen| runtime.follower_deadline(seen) > now);

    let mut snapshot = HaStatus {
        node_id: runtime.node_id.clone(),
        group_id: runtime.group_id.clone(),
        bind: runtime.bind.clone(),
        peer: runtime.peer.clone(),
        state: loop_state.state,
        owned: loop_state.ownership.owned(),
        resource,
        authority,
        safety,
        priority: runtime.priority,
        advert_interval_ms: runtime.advert_interval_ms,
        dead_timeout_ms: duration_millis_u64(runtime.dead_timeout()),
        hold_down_ms: runtime.hold_down_ms,
        sequence: loop_state.peer.last_sent_seq(),
        peer_node_id: loop_state
            .peer_observation
            .as_ref()
            .map(|peer| peer.node_id.clone()),
        peer_state: loop_state.peer_observation.as_ref().map(|peer| peer.state),
        peer_alive: live_peer.is_some(),
        last_peer_seen_ms_ago,
        packets_sent: loop_state.counters.packets_sent,
        packets_received: loop_state.counters.packets_received,
        invalid_packets: loop_state.counters.invalid_packets,
        auth_failures: loop_state.counters.auth_failures,
        duplicate_node_id_packets: loop_state.counters.duplicate_node_id_packets,
        decision_reason: loop_state.decision_reason,
        last_transition_reason: loop_state.last_transition_reason,
        last_transition_ms_ago: loop_state
            .last_transition_at
            .map(|transition_at| saturating_elapsed_ms(now, transition_at)),
        last_transition: loop_state
            .last_transition_reason
            .zip(loop_state.last_transition_at)
            .map(|(reason, transition_at)| LastTransitionStatus {
                reason,
                trigger: loop_state
                    .last_transition_trigger
                    .unwrap_or(TransitionTrigger::Election),
                ago_ms: saturating_elapsed_ms(now, transition_at),
            }),
        failure_policy: failure_policy_name(&runtime.failure_policy).to_owned(),
        peer_session: PeerSessionStatus {
            reachable: within_deadline(loop_state.peer.last_authenticated_at()),
            session_established: within_deadline(loop_state.peer.last_fresh_at()),
            rejections: loop_state.peer.rejections(),
        },
        watchdog: loop_state.ownership.watchdog_status(now.into()),
        lifecycle: LifecycleStatus::snapshot(
            loop_state.ownership.application(),
            loop_state.ownership.generation(),
            loop_state.ownership.verification_deadline(),
            now.into(),
        ),
        instance_id: loop_state.peer.instance_id(),
    };
    snapshot.apply_application_status(
        loop_state.ownership.application(),
        loop_state.ownership.generation(),
        loop_state.ownership.verification_deadline(),
        now.into(),
    );
    snapshot
}

fn publish_status(status_tx: Option<&watch::Sender<HaStatus>>, snapshot: HaStatus) {
    if let Some(status_tx) = status_tx {
        status_tx.send_replace(snapshot);
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn saturating_elapsed_ms(now: Instant, observed_at: Instant) -> u64 {
    duration_millis_u64(now.saturating_duration_since(observed_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gruezi::{
        application::{ApplicationOperation, ApplicationPhase, ApplicationState, CriticalFailure},
        hooks::{LifecycleKind, LifecycleOutcome},
        status::StatusResponse,
    };
    use anyhow::Result;

    fn application(phase: ApplicationPhase) -> ApplicationState {
        ApplicationState {
            phase,
            operation: None,
            last_critical_failure: None,
        }
    }

    fn ready(now: tokio::time::Instant) -> ApplicationState {
        application(ApplicationPhase::Ready {
            generation: 7,
            db_incarnation: Some("123:456".to_owned()),
            verified_at: now,
            next_verify_at: now + Duration::from_millis(100),
        })
    }

    fn status(application: &ApplicationState, now: tokio::time::Instant) -> HaStatus {
        let mut status = HaStatus::new("node-a", "group", "127.0.0.1:1", "127.0.0.1:2");
        status.state = HaState::Active;
        status.owned = true;
        status.apply_application_status(
            application,
            7,
            Some(now + Duration::from_millis(200)),
            now,
        );
        status
    }

    #[test]
    fn status_lifecycle_absent_reports_not_configured() {
        let status = HaStatus::new("a", "g", "127.0.0.1:1", "127.0.0.1:2");
        assert!(!status.lifecycle.configured);
        assert_eq!(status.lifecycle.phase, "not_configured");
        assert_eq!(status.lifecycle.verification.state, "not_configured");
        assert!(!status.lifecycle.confirmations.activation_confirmed);
        assert!(!status.lifecycle.confirmations.passive_confirmed);
        assert!(!status.lifecycle.confirmations.verified_fresh);
    }

    #[test]
    fn status_owned_false_while_activating() {
        let now = tokio::time::Instant::now();
        let mut application = application(ApplicationPhase::Activating { generation: 7 });
        application.operation = Some(ApplicationOperation {
            kind: LifecycleKind::Activate,
            generation: 7,
            submitted_at: now,
            deadline: now + Duration::from_millis(75),
        });
        let status = status(&application, now);
        assert_eq!(status.state, HaState::Active);
        assert!(!status.owned);
        assert_eq!(status.lifecycle.phase, "activating");
        assert_eq!(status.lifecycle.generation, 7);
        assert_eq!(
            status
                .lifecycle
                .operation
                .as_ref()
                .map(|op| (op.event.as_str(), op.remaining_deadline_ms)),
            Some(("activate", 75))
        );
        assert!(status.safety.why.contains("lifecycle"));
        assert!(!status.lifecycle.confirmations.activation_confirmed);
    }

    #[test]
    fn status_keeps_failure_after_successful_cleanup() {
        let now = tokio::time::Instant::now();
        let mut application = application(ApplicationPhase::Activating { generation: 7 });
        application.record_failure(&crate::gruezi::hooks::LifecycleEvent {
            kind: LifecycleKind::Activate,
            generation: 6,
            outcome: LifecycleOutcome::Failed,
            elapsed: Duration::from_millis(23),
            started: Some(now),
            exit_code_or_signal: Some(10),
            stderr_tail: "activation refused".to_owned(),
            db_incarnation: None,
        });
        let failure = status(&application, now).lifecycle.last_critical_failure;
        application.phase = ApplicationPhase::Inactive { generation: 7 };
        let status = status(&application, now);
        assert!(!status.owned);
        assert!(status.lifecycle.confirmations.passive_confirmed);
        assert_eq!(status.lifecycle.last_critical_failure, failure);
        assert_eq!(
            failure.as_ref().map(|failure| failure.event.as_str()),
            Some("activate")
        );
    }

    #[test]
    fn status_stderr_tail_is_bounded_and_sanitized() {
        let now = tokio::time::Instant::now();
        let mut application = application(ApplicationPhase::Uncertain { generation: 7 });
        application.last_critical_failure = Some(CriticalFailure {
            kind: LifecycleKind::Verify,
            generation: 7,
            outcome: LifecycleOutcome::TimedOut,
            elapsed: Duration::from_millis(100),
            exit_code_or_signal: Some(-9),
            stderr_tail: format!("{}\n\0\u{1b}\r\tend", "é".repeat(2_000)),
        });
        let status = status(&application, now);
        let failure = status.lifecycle.last_critical_failure.as_ref();
        assert!(
            failure.is_some_and(|failure| failure.stderr_tail.len() <= 1_024
                && !failure.stderr_tail.chars().any(char::is_control)
                && failure.stderr_tail.ends_with("end")
                && failure.event == "verify"
                && failure.outcome == "timed_out"
                && failure.elapsed_ms == 100
                && failure.exit_code_or_signal == Some(-9))
        );
        assert_eq!(status.lifecycle.verification.state, "failed");
        assert!(status.safety.why.contains("release"));
    }

    #[test]
    fn status_separates_activation_confirmed_from_verification_freshness() {
        let now = tokio::time::Instant::now();
        let mut application = ready(now);
        let deadline = now + Duration::from_millis(200);
        let mut status = status(&application, now);
        assert!(status.lifecycle.confirmations.activation_confirmed);
        assert!(status.lifecycle.confirmations.verified_fresh);
        application.operation = Some(ApplicationOperation {
            kind: LifecycleKind::Verify,
            generation: 7,
            submitted_at: now + Duration::from_millis(100),
            deadline,
        });
        status.apply_application_status(
            &application,
            7,
            Some(deadline),
            now + Duration::from_millis(110),
        );
        assert!(status.lifecycle.confirmations.activation_confirmed);
        assert!(status.lifecycle.confirmations.verified_fresh);
        assert_eq!(status.lifecycle.verification.state, "verifying");
        status.apply_application_status(&application, 7, Some(deadline), deadline);
        assert!(status.lifecycle.confirmations.activation_confirmed);
        assert!(!status.lifecycle.confirmations.verified_fresh);
        assert!(!status.owned);
    }

    #[test]
    fn status_reports_stale_verification() {
        let now = tokio::time::Instant::now();
        let application = ready(now);
        let mut status = status(&application, now);
        status.apply_application_status(
            &application,
            7,
            Some(now + Duration::from_millis(200)),
            now + Duration::from_millis(250),
        );
        assert_eq!(status.lifecycle.verification.state, "stale");
        assert_eq!(
            status.lifecycle.verification.last_verified_ago_ms,
            Some(250)
        );
        assert_eq!(status.lifecycle.verification.next_verify_in_ms, Some(0));
        assert_eq!(
            status.lifecycle.verification.db_incarnation.as_deref(),
            Some("123:456")
        );
        assert!(!status.owned);
        assert!(status.safety.why.contains("verification"));
    }

    fn application_step(
        application: &mut ApplicationState,
        phase: &str,
        verification: bool,
        now: tokio::time::Instant,
    ) -> tokio::time::Instant {
        let mut at = now;
        match phase {
            "activating" => application.phase = ApplicationPhase::Activating { generation: 7 },
            "ready" => *application = ready(now),
            "verifying" => {
                at = now + Duration::from_millis(110);
                application.operation = Some(ApplicationOperation {
                    kind: LifecycleKind::Verify,
                    generation: 7,
                    submitted_at: at,
                    deadline: now + Duration::from_millis(200),
                });
            }
            "stale" => at = now + Duration::from_millis(200),
            "failed" => {
                application.phase = ApplicationPhase::Deactivating { generation: 7 };
                application.operation = None;
                application.last_critical_failure = Some(CriticalFailure {
                    kind: if verification {
                        LifecycleKind::Verify
                    } else {
                        LifecycleKind::Activate
                    },
                    generation: 7,
                    outcome: LifecycleOutcome::Failed,
                    elapsed: Duration::from_millis(5),
                    exit_code_or_signal: Some(10),
                    stderr_tail: "operation refused".to_owned(),
                });
            }
            "inactive" => {
                application.phase = ApplicationPhase::Inactive { generation: 7 };
                application.operation = None;
            }
            _ => {}
        }
        at
    }

    /// Exercise the real API over loopback with the same application snapshots
    /// consumed by the runtime publisher. Runtime event delivery belongs to task 5.
    async fn api_timeline(verification: bool) -> Result<()> {
        use tokio::{net::TcpListener, sync::oneshot, time::timeout};
        let now = tokio::time::Instant::now();
        let inactive = application(ApplicationPhase::Inactive { generation: 7 });
        let (status_tx, _) = watch::channel(status(&inactive, now));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(crate::gruezi::api::run_ha_api(
            listener,
            status_tx.clone(),
            async {
                let _ = shutdown_rx.await;
            },
        ));
        let result = async {
            let client = reqwest::Client::new();
            let mut application = inactive;
            for phase in [
                "inactive",
                "activating",
                "ready",
                "verifying",
                "stale",
                "failed",
                "inactive",
            ] {
                let at = application_step(&mut application, phase, verification, now);
                let mut snapshot = status(&application, now);
                snapshot.apply_application_status(
                    &application,
                    7,
                    Some(now + Duration::from_millis(200)),
                    at,
                );
                status_tx.send_replace(snapshot.clone());
                for path in ["/ha/status", "/status"] {
                    let text = timeout(
                        Duration::from_secs(3),
                        client.get(format!("http://{addr}{path}")).send(),
                    )
                    .await??
                    .error_for_status()?
                    .text()
                    .await?;
                    println!("STATUS QA phase={phase} path={path} {text}");
                    let decoded = client
                        .get(format!("http://{addr}{path}"))
                        .send()
                        .await?
                        .error_for_status()?
                        .json::<StatusResponse>()
                        .await?;
                    assert_eq!(decoded.ha, Some(snapshot.clone()));
                }
                assert_eq!(
                    snapshot.lifecycle.confirmations.activation_confirmed,
                    matches!(phase, "ready" | "verifying" | "stale")
                );
                assert_eq!(
                    snapshot.lifecycle.confirmations.verified_fresh,
                    matches!(phase, "ready" | "verifying")
                );
                assert_eq!(snapshot.owned, matches!(phase, "ready" | "verifying"));
                if application.last_critical_failure.is_some() {
                    assert!(snapshot.lifecycle.last_critical_failure.is_some());
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let _ = shutdown_tx.send(());
        timeout(Duration::from_secs(3), task).await???;
        println!(
            "STATUS QA cleanup: API task joined; listener closed; no subprocesses or files created"
        );
        result
    }

    #[tokio::test]
    async fn status_api_reflects_lifecycle() -> Result<()> {
        api_timeline(false).await
    }

    #[tokio::test]
    async fn status_api_reflects_verification() -> Result<()> {
        api_timeline(true).await
    }
}
