use super::{
    config::HaRuntimeConfig,
    election::{HaDecisionReason, HaState},
    runtime::HaLoopState,
};
use crate::gruezi::ownership::{AuthoritySource, OwnershipController, ResourceState};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyStatus {
    pub level: SafetyLevel,
    pub why: String,
}

fn ownership_status(
    ownership: &OwnershipController,
    now: Instant,
) -> (ResourceStatus, AuthorityStatus, SafetyStatus) {
    let resource = ownership.resource_state();
    let authority = ownership.authority();
    let (level, why) = if ownership.owned() && authority.source() == Some(AuthoritySource::Assumed)
    {
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

    HaStatus {
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
        sequence: loop_state.sequence,
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
    }
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
