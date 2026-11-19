use crate::{
    config::FailurePolicy,
    gruezi::{
        addresses::{
            DesiredOwnership, ResourceCommand, ResourceReport, ResourceReportKind,
            ResourceWorkerHandle,
        },
        ha::{HaDecisionReason, HaRuntimeConfig, HaState},
        watchdog::{WatchdogDevice, WatchdogManager, WatchdogStatus},
    },
};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant};

/// Source of ownership authority (`peer | assumed | external`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthoritySource {
    Peer,
    Assumed,
    External,
}

impl AuthoritySource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Assumed => "assumed",
            Self::External => "external",
        }
    }
}

/// Ownership authority state (`none | pending | valid | uncertain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityState {
    None,
    Pending,
    Valid {
        source: AuthoritySource,
        term: u64,
        expiry: Option<Instant>,
    },
    Uncertain {
        source: Option<AuthoritySource>,
        term: u64,
        expiry: Option<Instant>,
    },
}

impl AuthorityState {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Valid { .. } => "valid",
            Self::Uncertain { .. } => "uncertain",
        }
    }

    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self, Self::Valid { .. })
    }

    #[must_use]
    pub const fn source(&self) -> Option<AuthoritySource> {
        match self {
            Self::Valid { source, .. } => Some(*source),
            Self::Uncertain { source, .. } => *source,
            Self::None | Self::Pending => Option::None,
        }
    }

    #[must_use]
    pub const fn term(&self) -> u64 {
        match self {
            Self::Valid { term, .. } | Self::Uncertain { term, .. } => *term,
            Self::None | Self::Pending => 0,
        }
    }

    #[must_use]
    pub const fn expiry(&self) -> Option<Instant> {
        match self {
            Self::Valid { expiry, .. } | Self::Uncertain { expiry, .. } => *expiry,
            Self::None | Self::Pending => Option::None,
        }
    }
}

/// Kernel read-back presence observation for configured VIP addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedPresence {
    Unknown,
    Absent,
    Partial,
    Present,
}

/// Lifecycle state of the VIP resource (`unmanaged | absent | acquiring | present | releasing | uncertain`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    Unmanaged,
    Absent,
    Acquiring {
        generation: u64,
    },
    Present {
        generation: u64,
    },
    Releasing {
        generation: u64,
    },
    Uncertain {
        last_observation: ObservedPresence,
        last_error: Option<String>,
    },
}

impl ResourceState {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unmanaged => "unmanaged",
            Self::Absent => "absent",
            Self::Acquiring { .. } => "acquiring",
            Self::Present { .. } => "present",
            Self::Releasing { .. } => "releasing",
            Self::Uncertain { .. } => "uncertain",
        }
    }

    #[must_use]
    pub const fn is_confirmed_present_or_unmanaged(&self) -> bool {
        matches!(self, Self::Present { .. } | Self::Unmanaged)
    }

    #[must_use]
    pub const fn is_confirmed_absent_or_unmanaged(&self) -> bool {
        matches!(self, Self::Absent | Self::Unmanaged)
    }
}

/// Policy evidence the coordinator feeds into authority evaluation every
/// iteration. It lives outside the provider because both facts are gathered
/// by the protocol layer and consumed by the policy layer (spec section 6).
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyInputs {
    /// Bilateral consent deadline (design spec 4.4): send time of the newest
    /// own advert the peer acknowledged in its most recent fresh `ack_seq`,
    /// plus the dead window. `None` until such an ack arrives.
    pub consent_until: Option<Instant>,
    /// BOOTSTRAP rule satisfied (design spec 3.1): fresh session, both nodes
    /// report the resource absent/unmanaged, the peer holds no authority and
    /// this node wins the rank via `outranks`. Priority alone never grants.
    pub bootstrap_ok: bool,
    /// A fresh release (or confirmed return) permits this acquisition.
    pub handoff_allowed: bool,
    /// A release remains irrevocable until a confirmed return, even under
    /// the available policy's assumed fallback (I8).
    pub handoff_blocked: bool,
}

/// A fence request is never evidence that the peer stopped serving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FencingState {
    #[default]
    None,
    Requested,
    Confirmed,
}

impl FencingState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requested => "requested",
            Self::Confirmed => "confirmed",
        }
    }
}

/// Volatile external evidence. A grant's deadline is anchored at command
/// start, and fencing proof is invalidated by fresh peer activity or revocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalAuthority {
    grant_until: Option<Instant>,
    fencing: FencingState,
    term: u64,
    active: bool,
}

impl ExternalAuthority {
    fn evaluate(
        &mut self,
        elected_role: HaState,
        ready: bool,
        inputs: PolicyInputs,
    ) -> AuthorityState {
        let was_active = self.active;
        let permitted = elected_role == HaState::Active && !inputs.handoff_blocked;
        let grant_valid = self.grant_until.is_some_and(|until| until > Instant::now());
        if permitted
            && ready
            && grant_valid
            && (was_active
                || inputs.bootstrap_ok
                || inputs.handoff_allowed
                || self.fencing == FencingState::Confirmed)
        {
            if !self.active {
                self.term = self.term.saturating_add(1);
            }
            self.active = true;
            return AuthorityState::Valid {
                source: AuthoritySource::External,
                term: self.term,
                expiry: self.grant_until,
            };
        }
        self.active = false;
        if was_active {
            self.fencing = FencingState::None;
        }
        if permitted {
            AuthorityState::Pending
        } else {
            AuthorityState::None
        }
    }
}

/// Ownership authority provider boundary.
///
/// `LocalTimeout` is the `assumed` provider: the existing local-timeout
/// election decision. `PeerConsent` is selected by `failure_policy` and
/// grants `peer` authority from bilateral consent (design spec 4.4): `strict`
/// fails closed when consent lapses, `available` falls back to `assumed`
/// semantics while consent is stale. `External` requires a live command grant
/// and release or confirmed fencing evidence. Authority never
/// survives a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipAuthority {
    External(ExternalAuthority),
    LocalTimeout {
        term: u64,
        active: bool,
    },
    PeerConsent {
        policy: FailurePolicy,
        term: u64,
        active: bool,
    },
}

impl Default for OwnershipAuthority {
    fn default() -> Self {
        Self::LocalTimeout {
            term: 0,
            active: false,
        }
    }
}

impl OwnershipAuthority {
    /// Evaluate authority from the current elected role, reconciliation
    /// readiness and the policy evidence for this iteration.
    #[must_use]
    pub fn evaluate(
        &mut self,
        elected_role: HaState,
        ready_to_authorize: bool,
        inputs: PolicyInputs,
    ) -> AuthorityState {
        match self {
            Self::External(external) => external.evaluate(elected_role, ready_to_authorize, inputs),
            Self::LocalTimeout { term, active } => {
                Self::evaluate_assumed(term, active, elected_role, ready_to_authorize)
            }
            Self::PeerConsent {
                policy,
                term,
                active,
            } => {
                if inputs.handoff_blocked {
                    *active = false;
                    return AuthorityState::None;
                }
                if elected_role != HaState::Active {
                    *active = false;
                    return AuthorityState::None;
                }
                if !ready_to_authorize {
                    *active = false;
                    return AuthorityState::Pending;
                }
                let was_active = *active;
                let consent_fresh = inputs
                    .consent_until
                    .is_some_and(|until| until > Instant::now());
                match policy {
                    FailurePolicy::Strict => {
                        // Silence NEVER authorizes: without fresh bilateral
                        // consent an existing grant lapses (the caller
                        // revokes and withdraws) and a pending one stays
                        // pending forever.
                        if consent_fresh
                            && (was_active || inputs.bootstrap_ok || inputs.handoff_allowed)
                        {
                            return Self::activate_peer(term, active, inputs.consent_until);
                        }
                        *active = false;
                        if was_active {
                            AuthorityState::None
                        } else {
                            AuthorityState::Pending
                        }
                    }
                    FailurePolicy::Available => {
                        if consent_fresh {
                            // Fresh consent proves the session: acquisition
                            // still needs the bootstrap rule, a continuing
                            // grant only needs the consent.
                            if was_active || inputs.bootstrap_ok || inputs.handoff_allowed {
                                return Self::activate_peer(term, active, inputs.consent_until);
                            }
                            *active = false;
                            return AuthorityState::Pending;
                        }
                        // Consent is stale: the assumed provider (today's
                        // local-timeout election decision) keeps the node
                        // available. Possible dual ownership is explicitly
                        // accepted for this policy.
                        Self::evaluate_assumed(term, active, elected_role, ready_to_authorize)
                    }
                    FailurePolicy::External => {
                        // External grants use the separate command provider.
                        *active = false;
                        AuthorityState::Pending
                    }
                }
            }
        }
    }

    fn evaluate_assumed(
        term: &mut u64,
        active: &mut bool,
        elected_role: HaState,
        ready_to_authorize: bool,
    ) -> AuthorityState {
        if elected_role != HaState::Active {
            *active = false;
            return AuthorityState::None;
        }
        if !ready_to_authorize {
            *active = false;
            return AuthorityState::Pending;
        }
        if !*active {
            *active = true;
            *term = term.saturating_add(1);
        }
        AuthorityState::Valid {
            source: AuthoritySource::Assumed,
            term: *term,
            expiry: Option::None,
        }
    }

    fn activate_peer(
        term: &mut u64,
        active: &mut bool,
        consent_until: Option<Instant>,
    ) -> AuthorityState {
        if !*active {
            *active = true;
            *term = term.saturating_add(1);
        }
        AuthorityState::Valid {
            source: AuthoritySource::Peer,
            term: *term,
            expiry: consent_until,
        }
    }

    /// Explicitly revoke authority.
    pub fn revoke(&mut self) {
        match self {
            Self::External(external) => {
                external.active = false;
                external.fencing = FencingState::None;
            }
            Self::LocalTimeout { active, .. } | Self::PeerConsent { active, .. } => {
                *active = false;
            }
        }
    }
}

/// Whether the node manages kernel VIP addresses or runs without a VIP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceManagement {
    Unmanaged,
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecyclePhase {
    Booting,
    Ready,
    RevokedSticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingPromotion {
    generation: u64,
    previous_state: HaState,
    reason: HaDecisionReason,
}

/// Immediate hook notification produced by an election/authority transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleTransitionEffect {
    None,
    AwaitingConfirmation,
    PromoteNow {
        previous_state: HaState,
        reason: HaDecisionReason,
    },
    Demote {
        previous_state: HaState,
        current_state: HaState,
        reason: HaDecisionReason,
    },
    Backup {
        previous_state: HaState,
        reason: HaDecisionReason,
    },
}

/// Hook notification produced when the resource worker completes an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportEffect {
    None,
    PromoteConfirmed {
        previous_state: HaState,
        reason: HaDecisionReason,
    },
    Fault {
        state: HaState,
        previous_state: HaState,
    },
}

/// Coordinator state machine separating elected role, ownership authority, and verified resource state.
#[derive(Debug)]
pub struct OwnershipController {
    management: ResourceManagement,
    elected_role: HaState,
    previous_role: HaState,
    authority_provider: OwnershipAuthority,
    authority: AuthorityState,
    policy_inputs: PolicyInputs,
    resource_state: ResourceState,
    generation: u64,
    lifecycle: LifecyclePhase,
    pending_promotion: Option<PendingPromotion>,
    promoted_generation: Option<u64>,
    last_observed_at: Option<Instant>,
    last_error: Option<String>,
    watchdog: WatchdogManager,
}

impl OwnershipController {
    /// Construct a new ownership controller for the configured address management mode.
    #[must_use]
    pub fn new(management: ResourceManagement) -> Self {
        let (resource_state, lifecycle) = match management {
            ResourceManagement::Unmanaged => (ResourceState::Unmanaged, LifecyclePhase::Ready),
            ResourceManagement::Managed => (
                ResourceState::Releasing { generation: 0 },
                LifecyclePhase::Booting,
            ),
        };

        Self {
            management,
            elected_role: HaState::Init,
            previous_role: HaState::Init,
            authority_provider: OwnershipAuthority::default(),
            authority: AuthorityState::None,
            policy_inputs: PolicyInputs::default(),
            resource_state,
            generation: 0,
            lifecycle,
            pending_promotion: None,
            promoted_generation: None,
            last_observed_at: None,
            last_error: None,
            watchdog: WatchdogManager::default(),
        }
    }

    /// Enqueue the initial startup reconciliation before any owned state can be advertised.
    pub fn start_reconciliation(&mut self, worker: &ResourceWorkerHandle) {
        worker.set_directive(self.generation, DesiredOwnership::Revoked);
        worker.send_command(ResourceCommand::Withdraw(self.generation));
    }

    /// True once startup reconciliation has confirmed initial VIP absence (or unmanaged).
    #[must_use]
    pub const fn is_startup_reconciled(&self) -> bool {
        !matches!(self.lifecycle, LifecyclePhase::Booting)
    }

    /// Role that may be advertised to peers; never advertises `Active` before startup reconciliation finishes.
    #[must_use]
    pub const fn advertised_role(&self) -> HaState {
        if !self.is_startup_reconciled() && matches!(self.elected_role, HaState::Active) {
            HaState::Standby
        } else {
            self.elected_role
        }
    }

    /// Current elected role (`init | standby | active`).
    #[must_use]
    pub const fn elected_role(&self) -> HaState {
        self.elected_role
    }

    /// Previous elected role recorded at the last role transition.
    #[must_use]
    pub const fn previous_role(&self) -> HaState {
        self.previous_role
    }

    /// Current ownership authority state (`none | pending | valid | uncertain`).
    #[must_use]
    pub const fn authority(&self) -> AuthorityState {
        self.authority
    }

    /// Current verified resource state (`unmanaged | absent | acquiring | present | releasing | uncertain`).
    #[must_use]
    pub const fn resource_state(&self) -> &ResourceState {
        &self.resource_state
    }

    /// Current ownership generation counter.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Timestamp of the most recent kernel observation from the resource worker.
    #[must_use]
    pub const fn last_observed_at(&self) -> Option<Instant> {
        self.last_observed_at
    }

    /// Diagnostic message from the most recent failed resource operation, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Section 1 ownership predicate, including required watchdog protection.
    #[must_use]
    pub fn owned(&self) -> bool {
        matches!(self.elected_role, HaState::Active)
            && self.authority.is_valid()
            && self.watchdog.ownership_allowed()
            && match self.resource_state {
                ResourceState::Present { generation } => generation == self.generation,
                ResourceState::Unmanaged => true,
                ResourceState::Absent
                | ResourceState::Acquiring { .. }
                | ResourceState::Releasing { .. }
                | ResourceState::Uncertain { .. } => false,
            }
    }

    pub(crate) fn configure_watchdog(&mut self, runtime: &HaRuntimeConfig, device: WatchdogDevice) {
        // The worker currently has fixed command/withdrawal bounds. Do not
        // under-budget them when a config requests shorter deadlines.
        let budget = Duration::from_millis(runtime.advert_interval_ms)
            .saturating_add(Duration::from_millis(
                runtime
                    .command_timeout_ms
                    .max(crate::gruezi::addresses::COMMAND_TIMEOUT_MS),
            ))
            .saturating_add(Duration::from_millis(
                runtime
                    .withdraw_timeout_ms
                    .max(crate::gruezi::addresses::WITHDRAW_TIMEOUT_MS),
            ))
            .saturating_add(Duration::from_millis(u64::from(
                runtime.watchdog.safety_margin_ms,
            )));
        self.watchdog = WatchdogManager::new(runtime.watchdog.clone(), device, budget);
    }

    pub(crate) fn watchdog_status(&self, now: Instant) -> WatchdogStatus {
        self.watchdog.status(now)
    }

    /// The only feeding entry point, called once per coordinator iteration.
    /// Share the owned predicate so both present and unmanaged resources feed.
    /// A failed keepalive revokes authority and starts bounded withdrawal.
    pub(crate) fn feed_watchdog(&mut self, worker: &ResourceWorkerHandle) -> RoleTransitionEffect {
        if self.owned()
            && self
                .authority
                .expiry()
                .is_none_or(|until| until > Instant::now())
            && let Err(error) = self.watchdog.feed(self.generation)
        {
            let effect = self.revoke_authority_internal(
                self.elected_role,
                self.elected_role,
                HaDecisionReason::RuntimeFault,
                worker,
            );
            self.last_error = Some(error.to_string());
            return effect;
        }
        RoleTransitionEffect::None
    }

    fn disarm_watchdog_after_absence(&mut self) {
        if let Err(error) = self.watchdog.disarm() {
            tracing::warn!(%error, "watchdog remains unfed after confirmed absence");
            self.last_error = Some(error.to_string());
        }
    }

    /// True only when authority is not valid and the resource is confirmed `absent` or `unmanaged` (I6).
    #[must_use]
    pub const fn can_release_handoff(&self) -> bool {
        !self.authority.is_valid()
            && matches!(self.lifecycle, LifecyclePhase::Ready)
            && self.resource_state.is_confirmed_absent_or_unmanaged()
    }

    /// Select the authority provider for the configured failure policy.
    /// `strict` and `available` are authorized by bilateral consent (with the
    /// documented `available` fallback); `external` requires command evidence.
    pub fn set_failure_policy(&mut self, policy: FailurePolicy) {
        self.authority_provider = if policy == FailurePolicy::External {
            OwnershipAuthority::External(ExternalAuthority::default())
        } else {
            OwnershipAuthority::PeerConsent {
                policy,
                term: 0,
                active: false,
            }
        };
    }

    /// Apply a bounded check result. Failure immediately removes the grant;
    /// even successful commands completing after their cutoff grant nothing.
    pub(crate) fn observe_external_grant(&mut self, expiry: Option<Instant>) {
        if let OwnershipAuthority::External(external) = &mut self.authority_provider {
            external.grant_until = expiry.filter(|until| *until > Instant::now());
        }
    }

    pub(crate) fn external_grant_expiry(&self) -> Option<Instant> {
        match &self.authority_provider {
            OwnershipAuthority::External(external) => external.grant_until,
            _ => None,
        }
    }

    pub(crate) fn fencing(&self) -> FencingState {
        match &self.authority_provider {
            OwnershipAuthority::External(external) => external.fencing,
            _ => FencingState::None,
        }
    }

    pub(crate) fn set_fencing(&mut self, state: FencingState) {
        if let OwnershipAuthority::External(external) = &mut self.authority_provider {
            external.fencing = state;
        }
    }

    /// Only a live grant may initiate fencing. Neither silence nor a previous
    /// grant's expiry is proof of release, including at lone-node startup.
    pub(crate) fn needs_fencing(&self) -> bool {
        matches!(&self.authority_provider, OwnershipAuthority::External(external)
            if !external.active
                && external.grant_until.is_some_and(|until| until > Instant::now())
                && external.fencing != FencingState::Confirmed)
            && self.elected_role == HaState::Active
            && matches!(self.lifecycle, LifecyclePhase::Ready)
            && !self.policy_inputs.bootstrap_ok
            && !self.policy_inputs.handoff_allowed
            && !self.policy_inputs.handoff_blocked
    }

    /// Record bilateral consent evidence (design spec 4.4): a fresh
    /// authenticated advert acknowledged one of our own sends. Consent is
    /// anchored at the SEND time of the newest acknowledged own sequence
    /// number, so loss in either direction stops consent from advancing and a
    /// delayed or replayed acknowledgement can never extend it past the
    /// formula. A fresh advert acknowledging none of our sends clears consent:
    /// a new session never inherits old authority.
    pub fn observe_consent(&mut self, acked_send_at: Option<Instant>, dead_window: Duration) {
        self.policy_inputs.consent_until = acked_send_at.map(|sent_at| sent_at + dead_window);
    }

    /// Feed this iteration's bootstrap-rule verdict (design spec 3.1).
    pub fn set_bootstrap_allowed(&mut self, bootstrap_ok: bool) {
        self.policy_inputs.bootstrap_ok = bootstrap_ok;
    }

    /// Apply the coordinator's handoff gate before evaluating any role or
    /// authority change. A blocked transfer cannot use timeout authority.
    pub fn set_handoff_gate(&mut self, allowed: bool, blocked: bool) {
        self.policy_inputs.handoff_allowed = allowed;
        self.policy_inputs.handoff_blocked = blocked;
    }

    /// Re-evaluate authority for the current elected role under the failure
    /// policy. The coordinator calls this every iteration and at the
    /// authority expiry deadline, so a lapsed consent is handled exactly on
    /// time: `strict` revokes and withdraws, `available` falls back to
    /// `assumed` without interrupting ownership.
    pub fn refresh_authority(&mut self, worker: &ResourceWorkerHandle) -> RoleTransitionEffect {
        if matches!(self.lifecycle, LifecyclePhase::Booting) {
            // Startup reconciliation still owes an absence proof; the
            // authority re-check in `handle_absent_report` runs once it lands.
            return RoleTransitionEffect::None;
        }
        if self.elected_role != HaState::Active {
            return RoleTransitionEffect::None;
        }
        if !matches!(self.lifecycle, LifecyclePhase::Ready) {
            // Sticky revocation: authority stays pending until withdrawal
            // confirms absence; a policy grant must never push past it (I7).
            self.authority =
                self.authority_provider
                    .evaluate(HaState::Active, false, self.policy_inputs);
            return RoleTransitionEffect::None;
        }
        let was_valid = self.authority.is_valid();
        let authority = self
            .authority_provider
            .evaluate(HaState::Active, true, self.policy_inputs);
        if authority.is_valid() {
            if was_valid {
                self.authority = authority;
                return self.feed_watchdog(worker);
            }
            let reason = self
                .pending_promotion
                .map_or(HaDecisionReason::AlreadyActive, |pending| pending.reason);
            return self.begin_acquisition(authority, reason, worker);
        }
        if was_valid {
            // Strict consent lapse: authority is revoked and the resource
            // worker withdraws (invariant I3). The elected role is untouched;
            // only ownership authority ends.
            return self.revoke_authority_internal(
                HaState::Active,
                HaState::Active,
                HaDecisionReason::ConsentExpired,
                worker,
            );
        }
        self.authority = authority;
        RoleTransitionEffect::None
    }

    /// Start (or restart) an acquisition for an already-computed authority
    /// state. Never touches the resource while authority is pending (I1).
    fn begin_acquisition(
        &mut self,
        authority: AuthorityState,
        reason: HaDecisionReason,
        worker: &ResourceWorkerHandle,
    ) -> RoleTransitionEffect {
        self.generation = self.generation.saturating_add(1);
        self.authority = authority;
        if !self.authority.is_valid() {
            self.pending_promotion = Some(PendingPromotion {
                generation: self.generation,
                previous_state: self.previous_role,
                reason,
            });
            return RoleTransitionEffect::AwaitingConfirmation;
        }
        if !self.resource_state.is_confirmed_absent_or_unmanaged() {
            self.authority_provider.revoke();
            self.authority = AuthorityState::Pending;
            return RoleTransitionEffect::AwaitingConfirmation;
        }
        if let Err(error) = self.watchdog.arm(
            self.generation,
            self.last_observed_at.is_some() && matches!(self.lifecycle, LifecyclePhase::Ready),
        ) {
            // Keep the worker revoked: neither a direct Acquire nor periodic
            // Reconcile may add a VIP after an unsuccessful arm (I10).
            self.authority_provider.revoke();
            self.authority = AuthorityState::Pending;
            self.last_error = Some(error.to_string());
            return RoleTransitionEffect::AwaitingConfirmation;
        }
        if self.management == ResourceManagement::Unmanaged {
            self.resource_state = ResourceState::Unmanaged;
            self.promoted_generation = Some(self.generation);
            self.pending_promotion = None;
            return RoleTransitionEffect::PromoteNow {
                previous_state: self.previous_role,
                reason,
            };
        }
        self.pending_promotion = Some(PendingPromotion {
            generation: self.generation,
            previous_state: self.previous_role,
            reason,
        });
        self.resource_state = ResourceState::Acquiring {
            generation: self.generation,
        };
        worker.set_directive(self.generation, DesiredOwnership::Authorized);
        worker.send_command(ResourceCommand::Acquire(self.generation));
        RoleTransitionEffect::AwaitingConfirmation
    }

    /// Set the elected role while holding authority in `Pending` (for testing invariant I1).
    pub fn set_elected_role_pending(&mut self, role: HaState, reason: HaDecisionReason) {
        self.previous_role = self.elected_role;
        self.elected_role = role;
        self.authority_provider.revoke();
        self.authority = if role == HaState::Active {
            AuthorityState::Pending
        } else {
            AuthorityState::None
        };
        if role == HaState::Active {
            self.pending_promotion = Some(PendingPromotion {
                generation: self.generation,
                previous_state: self.previous_role,
                reason,
            });
        } else {
            self.pending_promotion = None;
        }
    }

    /// Apply an election decision and enqueue any required serialized resource worker command.
    pub fn apply_role_decision(
        &mut self,
        next_role: HaState,
        reason: HaDecisionReason,
        worker: &ResourceWorkerHandle,
    ) -> RoleTransitionEffect {
        if next_role == self.elected_role {
            return RoleTransitionEffect::None;
        }

        let previous_role = self.elected_role;
        self.previous_role = previous_role;
        self.elected_role = next_role;

        match next_role {
            HaState::Standby if previous_role == HaState::Init => {
                self.authority =
                    self.authority_provider
                        .evaluate(next_role, true, self.policy_inputs);
                RoleTransitionEffect::Backup {
                    previous_state: previous_role,
                    reason,
                }
            }
            HaState::Active => {
                if !matches!(self.lifecycle, LifecyclePhase::Ready) {
                    self.authority = self.authority_provider.evaluate(
                        HaState::Active,
                        false,
                        self.policy_inputs,
                    );
                    self.pending_promotion = Some(PendingPromotion {
                        generation: self.generation,
                        previous_state: previous_role,
                        reason,
                    });
                    if matches!(self.lifecycle, LifecyclePhase::RevokedSticky) {
                        worker.set_directive(self.generation, DesiredOwnership::Revoked);
                        worker.send_command(ResourceCommand::Acquire(
                            self.generation.saturating_add(1),
                        ));
                    }
                    return RoleTransitionEffect::AwaitingConfirmation;
                }

                let authority =
                    self.authority_provider
                        .evaluate(HaState::Active, true, self.policy_inputs);
                self.begin_acquisition(authority, reason, worker)
            }
            HaState::Standby | HaState::Init => {
                self.revoke_authority_internal(previous_role, next_role, reason, worker)
            }
        }
    }

    /// Revoke authority immediately and enqueue a sticky withdrawal on the resource worker.
    pub fn revoke_authority(
        &mut self,
        reason: HaDecisionReason,
        worker: &ResourceWorkerHandle,
    ) -> RoleTransitionEffect {
        let previous_role = self.elected_role;
        self.previous_role = previous_role;
        self.elected_role = HaState::Standby;
        self.revoke_authority_internal(previous_role, HaState::Standby, reason, worker)
    }

    fn revoke_authority_internal(
        &mut self,
        previous_role: HaState,
        current_state: HaState,
        reason: HaDecisionReason,
        worker: &ResourceWorkerHandle,
    ) -> RoleTransitionEffect {
        self.watchdog.stop_feeding();
        self.authority_provider.revoke();
        self.authority = AuthorityState::None;
        self.pending_promotion = None;
        self.generation = self.generation.saturating_add(1);

        if self.management == ResourceManagement::Managed {
            self.lifecycle = LifecyclePhase::RevokedSticky;
            self.resource_state = ResourceState::Releasing {
                generation: self.generation,
            };
            worker.set_directive(self.generation, DesiredOwnership::Revoked);
            worker.send_command(ResourceCommand::Withdraw(self.generation));
        } else {
            self.resource_state = ResourceState::Unmanaged;
            self.disarm_watchdog_after_absence();
        }

        if previous_role == HaState::Active {
            RoleTransitionEffect::Demote {
                previous_state: previous_role,
                current_state,
                reason,
            }
        } else {
            RoleTransitionEffect::None
        }
    }

    fn consume_pending_promotion(&mut self, generation: u64) -> ReportEffect {
        if self.promoted_generation != Some(generation) {
            self.promoted_generation = Some(generation);
            let (previous_state, reason) = self
                .pending_promotion
                .take()
                .map_or((self.previous_role, HaDecisionReason::AlreadyActive), |p| {
                    (p.previous_state, p.reason)
                });
            return ReportEffect::PromoteConfirmed {
                previous_state,
                reason,
            };
        }
        ReportEffect::None
    }

    fn handle_absent_report(&mut self, worker: &ResourceWorkerHandle) -> ReportEffect {
        let was_booting = matches!(self.lifecycle, LifecyclePhase::Booting);
        self.lifecycle = LifecyclePhase::Ready;
        self.resource_state = ResourceState::Absent;
        if !self.authority.is_valid() {
            self.disarm_watchdog_after_absence();
        }

        if was_booting && self.elected_role == HaState::Active {
            // Startup reconciliation confirmed absence: acquisition begins
            // only when the failure policy granted authority (bootstrap rule).
            let reason = self
                .pending_promotion
                .map_or(HaDecisionReason::AlreadyActive, |pending| pending.reason);
            let authority =
                self.authority_provider
                    .evaluate(HaState::Active, true, self.policy_inputs);
            return match self.begin_acquisition(authority, reason, worker) {
                RoleTransitionEffect::PromoteNow {
                    previous_state,
                    reason,
                } => ReportEffect::PromoteConfirmed {
                    previous_state,
                    reason,
                },
                RoleTransitionEffect::None
                | RoleTransitionEffect::AwaitingConfirmation
                | RoleTransitionEffect::Demote { .. }
                | RoleTransitionEffect::Backup { .. } => ReportEffect::None,
            };
        }

        ReportEffect::None
    }

    fn handle_present_report(
        &mut self,
        generation: u64,
        worker: &ResourceWorkerHandle,
    ) -> ReportEffect {
        let valid_for_current = generation == self.generation
            && self.elected_role == HaState::Active
            && self.authority.is_valid()
            && self
                .authority
                .expiry()
                .is_none_or(|until| until > Instant::now())
            && matches!(self.lifecycle, LifecyclePhase::Ready);

        if !valid_for_current {
            // Invariant I7: a stale completion never restores ownership and triggers compensating withdrawal.
            self.lifecycle = LifecyclePhase::RevokedSticky;
            self.watchdog.stop_feeding();
            self.resource_state = ResourceState::Releasing {
                generation: self.generation,
            };
            worker.set_directive(self.generation, DesiredOwnership::Revoked);
            worker.send_command(ResourceCommand::Withdraw(self.generation));
            return ReportEffect::None;
        }

        self.resource_state = ResourceState::Present { generation };
        self.consume_pending_promotion(generation)
    }

    /// Apply a completed report from the resource worker and determine whether any hook should fire.
    pub fn apply_worker_report(
        &mut self,
        report: ResourceReport,
        worker: &ResourceWorkerHandle,
    ) -> ReportEffect {
        // An observation queued before revocation cannot prove that the
        // current generation drained its mutations and withdrew (I6/I7).
        if report.generation != self.generation
            && matches!(
                report.resource_state,
                ResourceState::Absent | ResourceState::Unmanaged
            )
        {
            return ReportEffect::None;
        }
        self.last_observed_at = Some(report.observed_at);
        self.last_error.clone_from(&report.error);

        match report.resource_state {
            ResourceState::Unmanaged => {
                self.lifecycle = LifecyclePhase::Ready;
                self.resource_state = ResourceState::Unmanaged;
                if !self.authority.is_valid() {
                    self.disarm_watchdog_after_absence();
                }
                if self.elected_role == HaState::Active && self.authority.is_valid() {
                    return self.consume_pending_promotion(self.generation);
                }
                ReportEffect::None
            }
            ResourceState::Absent => self.handle_absent_report(worker),
            ResourceState::Present { generation } => self.handle_present_report(generation, worker),
            ResourceState::Acquiring { generation } => {
                if generation == self.generation {
                    self.resource_state = ResourceState::Acquiring { generation };
                }
                ReportEffect::None
            }
            ResourceState::Releasing { generation } => {
                self.watchdog.stop_feeding();
                self.resource_state = ResourceState::Releasing { generation };
                ReportEffect::None
            }
            ResourceState::Uncertain {
                last_observation,
                last_error,
            } => {
                if matches!(self.lifecycle, LifecyclePhase::Booting) {
                    self.lifecycle = LifecyclePhase::Ready;
                }
                self.resource_state = ResourceState::Uncertain {
                    last_observation,
                    last_error,
                };
                if report.kind == ResourceReportKind::Fault {
                    ReportEffect::Fault {
                        state: self.elected_role,
                        previous_state: self.previous_role,
                    }
                } else {
                    ReportEffect::None
                }
            }
        }
    }

    /// Trigger a reconciliation check on the single resource worker.
    pub fn request_reconcile(&self, worker: &ResourceWorkerHandle) {
        let desired = if self.elected_role == HaState::Active
            && self.authority.is_valid()
            && matches!(self.lifecycle, LifecyclePhase::Ready)
        {
            DesiredOwnership::Authorized
        } else {
            DesiredOwnership::Revoked
        };
        worker.set_directive(self.generation, desired);
        worker.send_command(ResourceCommand::Reconcile);
    }

    /// Revoke authority, drain in-flight work, withdraw VIPs, and join the resource worker.
    pub async fn shutdown(&mut self, worker: &mut ResourceWorkerHandle) -> ResourceReport {
        self.watchdog.stop_feeding();
        self.authority_provider.revoke();
        self.authority = AuthorityState::None;
        self.pending_promotion = None;
        self.elected_role = HaState::Init;
        self.generation = self.generation.saturating_add(1);
        if self.management == ResourceManagement::Managed {
            self.lifecycle = LifecyclePhase::RevokedSticky;
            self.resource_state = ResourceState::Releasing {
                generation: self.generation,
            };
        }
        let report = worker.shutdown(self.generation).await;
        self.last_observed_at = Some(report.observed_at);
        self.last_error.clone_from(&report.error);
        self.resource_state = report.resource_state.clone();
        if self.resource_state.is_confirmed_absent_or_unmanaged() {
            self.lifecycle = LifecyclePhase::Ready;
            self.disarm_watchdog_after_absence();
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthoritySource, AuthorityState, ObservedPresence, OwnershipController, ReportEffect,
        ResourceManagement, ResourceState, RoleTransitionEffect,
    };
    use crate::gruezi::{
        addresses::{
            AddressManager, CommandGate, CommandRunner, DesiredOwnership, ResourceCommand,
            ResourceReport, ResourceReportKind, ResourceWorkerHandle, SimulatedKernel,
        },
        ha::{HaDecisionReason, HaState},
    };
    use anyhow::{Context, Result};
    use std::sync::{Arc, Mutex};
    use tokio::{
        sync::oneshot,
        time::{Duration, Instant},
    };

    fn managed_address_manager(addresses: Vec<&str>) -> AddressManager {
        AddressManager {
            ip_command: "ip".to_owned(),
            arping_command: "arping".to_owned(),
            ndsend_command: "ndsend".to_owned(),
            interface: "eth0".to_owned(),
            addresses: addresses.into_iter().map(str::to_owned).collect(),
        }
    }

    fn record_role_effect(effect: RoleTransitionEffect, hooks: &mut Vec<String>) {
        match effect {
            RoleTransitionEffect::PromoteNow { reason, .. } => {
                hooks.push(format!("promote:{reason}"));
            }
            RoleTransitionEffect::Demote { reason, .. } => {
                hooks.push(format!("demote:{reason}"));
            }
            RoleTransitionEffect::Backup { reason, .. } => {
                hooks.push(format!("backup:{reason}"));
            }
            RoleTransitionEffect::None | RoleTransitionEffect::AwaitingConfirmation => {}
        }
    }

    fn record_report_effect(effect: ReportEffect, hooks: &mut Vec<String>) {
        match effect {
            ReportEffect::PromoteConfirmed { reason, .. } => {
                hooks.push(format!("promote:{reason}"));
            }
            ReportEffect::Fault { state, .. } => {
                hooks.push(format!("fault:{state:?}"));
            }
            ReportEffect::None => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn elected_without_valid_authority_never_acquires_or_promotes() {
        // Invariant I1: Election alone never creates ownership (no add, no OWNED, no promote hook while authority pending).
        let kernel = SimulatedKernel::new_shared();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.fail_all_commands = true;
        }
        let mut controller = OwnershipController::new(ResourceManagement::Managed);
        controller.set_elected_role_pending(HaState::Active, HaDecisionReason::LocalHigherPriority);

        assert_eq!(controller.elected_role(), HaState::Active);
        assert_eq!(controller.authority(), AuthorityState::Pending);
        assert!(!controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        assert_eq!(
            SimulatedKernel::calls_snapshot(&kernel),
            Vec::<Vec<String>>::new()
        );
    }

    async fn boot_managed_pair(
        addresses: Vec<&str>,
    ) -> Result<(
        Arc<Mutex<SimulatedKernel>>,
        ResourceWorkerHandle,
        OwnershipController,
        Vec<String>,
    )> {
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(addresses);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);
        let mut hooks = Vec::new();
        controller.start_reconciliation(&worker);
        record_role_effect(
            controller.apply_role_decision(
                HaState::Standby,
                HaDecisionReason::StartupHold,
                &worker,
            ),
            &mut hooks,
        );
        let startup_report = worker
            .recv_report()
            .await
            .context("expected startup report")?;
        record_report_effect(
            controller.apply_worker_report(startup_report, &worker),
            &mut hooks,
        );
        Ok((kernel, worker, controller, hooks))
    }

    async fn recv_and_apply_report(
        worker: &mut ResourceWorkerHandle,
        controller: &mut OwnershipController,
        hooks: &mut Vec<String>,
    ) -> Result<ResourceReport> {
        let report = worker
            .recv_report()
            .await
            .context("expected worker report")?;
        record_report_effect(
            controller.apply_worker_report(report.clone(), worker),
            hooks,
        );
        Ok(report)
    }

    #[tokio::test(start_paused = true)]
    async fn stale_absence_cannot_confirm_a_new_handoff_generation() -> Result<()> {
        let (kernel, mut worker, mut controller, mut hooks) =
            boot_managed_pair(vec!["10.0.0.10/24"]).await?;
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        let old_generation = controller.generation();
        controller.revoke_authority(HaDecisionReason::GracefulShutdown, &worker);
        controller.apply_worker_report(
            ResourceReport {
                generation: old_generation,
                kind: ResourceReportKind::Reconciled,
                resource_state: ResourceState::Absent,
                observed_at: Instant::now(),
                error: None,
            },
            &worker,
        );
        assert!(
            !controller.can_release_handoff(),
            "I6: stale absence cannot prove withdrawal"
        );
        assert_ne!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        assert!(controller.can_release_handoff());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn happy_promote_then_demote_matches_simulated_kernel_table() -> Result<()> {
        // Manual-QA happy scenario + Invariants I1, I6, I9:
        // Simulated kernel table matches resource_state at every step; owned=true only after read-back confirms presence.
        let (kernel, mut worker, mut controller, mut hooks) =
            boot_managed_pair(vec!["10.0.0.10/24", "fd00::10/64"]).await?;
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert!(!controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        // Promote to Active: before worker report arrives, resource_state is Acquiring and owned is false.
        record_role_effect(
            controller.apply_role_decision(
                HaState::Active,
                HaDecisionReason::LocalHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Acquiring { generation: 1 }
        );
        assert_eq!(
            controller.authority(),
            AuthorityState::Valid {
                source: AuthoritySource::Assumed,
                term: 1,
                expiry: None,
            }
        );
        assert!(!controller.owned());
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));

        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        let table_after_promote = SimulatedKernel::table_snapshot(&kernel);
        println!("HAPPY QA - AFTER PROMOTE TABLE: {table_after_promote:?}");
        println!(
            "HAPPY QA - AFTER PROMOTE STATE: resource_state={:?} owned={} hooks={hooks:?}",
            controller.resource_state(),
            controller.owned()
        );
        assert_eq!(
            table_after_promote,
            vec![
                "eth0 10.0.0.10/24".to_owned(),
                "eth0 fd00::10/64".to_owned()
            ]
        );
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Present { generation: 1 }
        );
        assert!(controller.owned());
        assert_eq!(
            hooks,
            vec![
                "backup:startup_hold".to_owned(),
                "promote:local_higher_priority".to_owned()
            ]
        );

        // Demote to Standby: authority revoked immediately, on_demote fires, releasing until read-back confirms absence.
        record_role_effect(
            controller.apply_role_decision(
                HaState::Standby,
                HaDecisionReason::PeerHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Releasing { generation: 2 }
        );
        assert!(!controller.owned());
        assert!(!controller.can_release_handoff());

        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        let table_after_demote = SimulatedKernel::table_snapshot(&kernel);
        println!("HAPPY QA - AFTER DEMOTE TABLE: {table_after_demote:?}");
        println!(
            "HAPPY QA - AFTER DEMOTE STATE: resource_state={:?} owned={} can_release={} hooks={hooks:?}",
            controller.resource_state(),
            controller.owned(),
            controller.can_release_handoff()
        );
        assert_eq!(table_after_demote, Vec::<String>::new());
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert!(!controller.owned());
        assert!(controller.can_release_handoff());
        assert_eq!(
            hooks,
            vec![
                "backup:startup_hold".to_owned(),
                "promote:local_higher_priority".to_owned(),
                "demote:peer_higher_priority".to_owned(),
            ]
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_add_then_demote_ends_absent() -> Result<()> {
        // Manual-QA failure scenario + Invariants I1, I6, I7:
        // Gate the add via Scripted/Simulated runner, demote, release the gate:
        // late add is compensated, final kernel table is absent, resource_state is absent,
        // and no promote hook fired for the old generation.
        let (kernel, mut worker, mut controller, mut hooks) =
            boot_managed_pair(vec!["10.0.0.10/24", "fd00::10/64"]).await?;

        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.gate_after_add = Some(CommandGate {
                entered: entered_tx,
                release: release_rx,
            });
        }

        // Start promotion (generation 1); the worker adds the first address and pauses at gate_after_add.
        record_role_effect(
            controller.apply_role_decision(
                HaState::Active,
                HaDecisionReason::LocalHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        entered_rx.await.context("worker should enter add gate")?;
        let table_while_gated = SimulatedKernel::table_snapshot(&kernel);
        println!("FAILURE QA - WHILE GATED TABLE: {table_while_gated:?}");
        println!(
            "FAILURE QA - WHILE GATED STATE: resource_state={:?} owned={} hooks={hooks:?}",
            controller.resource_state(),
            controller.owned()
        );
        assert_eq!(table_while_gated, vec!["eth0 10.0.0.10/24".to_owned()]);
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Acquiring { generation: 1 }
        );
        assert!(!controller.owned());
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));

        // Demote before the gated add completes (generation advances to 2, authority revoked).
        record_role_effect(
            controller.apply_role_decision(
                HaState::Standby,
                HaDecisionReason::PeerHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        assert_eq!(controller.generation(), 2);
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Releasing { generation: 2 }
        );
        assert!(!controller.owned());

        // Release the gate: the worker detects the generation change after the first add,
        // skips the second add, and executes a compensating withdrawal.
        let _ = release_tx.send(Ok(()));
        let report = worker
            .recv_report()
            .await
            .context("expected compensated withdrawal report")?;
        assert_eq!(report.kind, ResourceReportKind::CompensatedWithdrawal);
        record_report_effect(controller.apply_worker_report(report, &worker), &mut hooks);

        let final_table = SimulatedKernel::table_snapshot(&kernel);
        let calls = SimulatedKernel::calls_snapshot(&kernel);
        println!("FAILURE QA - FINAL SIMULATED KERNEL TABLE: {final_table:?}");
        println!(
            "FAILURE QA - FINAL STATE: resource_state={:?} owned={} can_release={} hooks={hooks:?}",
            controller.resource_state(),
            controller.owned(),
            controller.can_release_handoff()
        );
        assert_eq!(final_table, Vec::<String>::new());
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert!(!controller.owned());
        assert!(controller.can_release_handoff());
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));
        assert_eq!(
            hooks,
            vec![
                "backup:startup_hold".to_owned(),
                "demote:peer_higher_priority".to_owned(),
            ]
        );
        assert!(
            !calls
                .iter()
                .any(|call| call == &vec!["ip", "address", "add", "fd00::10/64", "dev", "eth0"])
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_add_drains_and_withdraws() -> Result<()> {
        // Invariant I7 & Spec §5.5: Shutdown while an add is in flight drains the add, withdraws all VIPs, and confirms absence.
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(vec!["10.0.0.10/24", "fd00::10/64"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);

        controller.start_reconciliation(&worker);
        let startup_report = worker
            .recv_report()
            .await
            .context("expected startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.gate_after_add = Some(CommandGate {
                entered: entered_tx,
                release: release_rx,
            });
        }

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::StartupDeadlineExpired,
            &worker,
        );
        entered_rx.await.context("worker should enter add gate")?;
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );

        let release_future = async move {
            tokio::task::yield_now().await;
            let _ = release_tx.send(Ok(()));
        };

        let (shutdown_report, ()) = tokio::join!(controller.shutdown(&mut worker), release_future);

        assert_eq!(shutdown_report.resource_state, ResourceState::Absent);
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert!(!controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn stale_completion_never_restores_ownership() -> Result<()> {
        // Invariant I7: Even if a stale Present report from an older generation reaches the controller
        // after authority was revoked, it never restores ownership and triggers compensating withdrawal.
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(vec!["10.0.0.10/24"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);
        let mut hooks = Vec::new();

        controller.start_reconciliation(&worker);
        let startup_report = worker
            .recv_report()
            .await
            .context("expected startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        // Begin generation 1 acquisition, then revoke authority (generation 2).
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        assert_eq!(controller.generation(), 1);
        record_role_effect(
            controller.revoke_authority(HaDecisionReason::PeerHigherPriority, &worker),
            &mut hooks,
        );
        assert_eq!(controller.generation(), 2);

        // Drain the worker's actual withdrawal report first, then inject a synthetic stale Present(generation: 1)
        // with a residual VIP in the simulated kernel table to verify controller-level I7 defense-in-depth.
        let initial_withdraw = worker
            .recv_report()
            .await
            .context("expected initial withdraw")?;
        controller.apply_worker_report(initial_withdraw, &worker);
        SimulatedKernel::insert_address(&kernel, "eth0", "10.0.0.10/24", "");

        let stale_report = ResourceReport {
            generation: 1,
            kind: ResourceReportKind::Acquired,
            resource_state: ResourceState::Present { generation: 1 },
            observed_at: Instant::now(),
            error: None,
        };
        record_report_effect(
            controller.apply_worker_report(stale_report, &worker),
            &mut hooks,
        );
        assert!(!controller.owned());
        assert_ne!(
            controller.resource_state(),
            &ResourceState::Present { generation: 1 }
        );
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));

        // The controller enqueued a compensating withdrawal that removes the stale VIP.
        let compensated = worker
            .recv_report()
            .await
            .context("expected compensating withdrawal")?;
        controller.apply_worker_report(compensated, &worker);
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn residual_vip_at_startup_is_removed_before_advertising() -> Result<()> {
        // Spec §5.2 & Invariant I1: Residual managed VIP present at startup is removed and absence is
        // confirmed before advertising any owned/Active state or starting an acquisition.
        let kernel = SimulatedKernel::new_shared();
        SimulatedKernel::insert_address(&kernel, "eth0", "10.0.0.10/24", "");
        SimulatedKernel::insert_address(&kernel, "eth0", "fd00::10/64", "");

        let manager = managed_address_manager(vec!["10.0.0.10/24", "fd00::10/64"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);

        controller.start_reconciliation(&worker);
        // Even if election selects Active before startup reconciliation completes,
        // the controller stays in AuthorityState::Pending and advertises Standby (not Active).
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        assert!(!controller.is_startup_reconciled());
        assert_eq!(controller.advertised_role(), HaState::Standby);
        assert_eq!(controller.authority(), AuthorityState::Pending);
        assert!(!controller.owned());

        // Startup reconciliation removes the residual VIPs first.
        let startup_report = worker
            .recv_report()
            .await
            .context("expected startup reconciliation report")?;
        assert_eq!(startup_report.resource_state, ResourceState::Absent);
        controller.apply_worker_report(startup_report, &worker);
        assert!(controller.is_startup_reconciled());
        assert_eq!(controller.advertised_role(), HaState::Active);

        // Only after startup absence was confirmed does the controller acquire for generation 1.
        let acquire_report = worker
            .recv_report()
            .await
            .context("expected post-reconcile acquire report")?;
        controller.apply_worker_report(acquire_report, &worker);
        assert!(controller.owned());
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Present { generation: 1 }
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn externally_removed_vip_is_readded_while_authority_is_valid_and_not_after_revocation()
    -> Result<()> {
        // Spec §5.2: Periodic reconciliation (every advert_interval_ms * dead_factor) re-adds an
        // externally removed VIP while authority is valid, and never re-adds it after revocation.
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(vec!["10.0.0.10/24"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);

        controller.start_reconciliation(&worker);
        let startup_report = worker.recv_report().await.context("startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        let acquire_report = worker.recv_report().await.context("acquire report")?;
        controller.apply_worker_report(acquire_report, &worker);
        assert!(controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );

        // Simulate an external operator/tool deleting the VIP from the kernel.
        SimulatedKernel::remove_address(&kernel, "eth0", "10.0.0.10/24");
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        // Advance the paused clock by advert_interval_ms * dead_factor (1000ms * 3 = 3000ms) and reconcile.
        tokio::time::advance(Duration::from_millis(3000)).await;
        controller.request_reconcile(&worker);
        let reconcile_report = worker.recv_report().await.context("reconcile report")?;
        controller.apply_worker_report(reconcile_report, &worker);
        assert!(controller.owned());
        assert_eq!(
            controller.resource_state(),
            &ResourceState::Present { generation: 1 }
        );
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );

        // Revoke authority (demote) and confirm withdrawal.
        controller.apply_role_decision(
            HaState::Standby,
            HaDecisionReason::PeerHigherPriority,
            &worker,
        );
        let withdraw_report = worker.recv_report().await.context("withdraw report")?;
        controller.apply_worker_report(withdraw_report, &worker);
        assert!(!controller.owned());
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        // Advance the paused clock by another advert_interval_ms * dead_factor and reconcile:
        // must NOT re-add the VIP after authority revocation.
        tokio::time::advance(Duration::from_millis(3000)).await;
        controller.request_reconcile(&worker);
        let post_revoke_reconcile = worker
            .recv_report()
            .await
            .context("post-revoke reconcile report")?;
        controller.apply_worker_report(post_revoke_reconcile, &worker);
        assert!(!controller.owned());
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn partial_acquisition_yields_uncertain_never_present() -> Result<()> {
        // Invariant I9: Partial acquisition (one address fails, or IPv6 is tentative) yields
        // ResourceState::Uncertain { last_observation: ObservedPresence::Partial, .. }, never Present.
        let kernel = SimulatedKernel::new_shared();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.fail_add_addresses.push("fd00::10/64".to_owned());
        }
        let manager = managed_address_manager(vec!["10.0.0.10/24", "fd00::10/64"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);
        let mut hooks = Vec::new();

        controller.start_reconciliation(&worker);
        let startup_report = worker.recv_report().await.context("startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        let partial_report = worker.recv_report().await.context("partial report")?;
        record_report_effect(
            controller.apply_worker_report(partial_report, &worker),
            &mut hooks,
        );

        assert!(!controller.owned());
        assert!(matches!(
            controller.resource_state(),
            ResourceState::Uncertain {
                last_observation: ObservedPresence::Partial,
                last_error: Some(_),
            }
        ));
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));
        assert_eq!(hooks, vec!["fault:Active".to_owned()]);

        // Now allow the IPv6 add to succeed, but mark it tentative in kernel read-back: still Partial/Uncertain!
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.fail_add_addresses.clear();
            guard
                .default_add_flags
                .push(("fd00::10/64".to_owned(), "tentative".to_owned()));
        }
        controller.request_reconcile(&worker);
        let tentative_report = worker.recv_report().await.context("tentative report")?;
        record_report_effect(
            controller.apply_worker_report(tentative_report, &worker),
            &mut hooks,
        );
        assert!(!controller.owned());
        assert!(matches!(
            controller.resource_state(),
            ResourceState::Uncertain {
                last_observation: ObservedPresence::Partial,
                last_error: Some(_),
            }
        ));
        assert!(!hooks.iter().any(|entry| entry.starts_with("promote:")));

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn revocation_is_sticky_until_absence_confirmed() -> Result<()> {
        // Invariants I6 & I8: Revocation/Withdraw is sticky; queued Acquires are dropped and
        // later Acquires are rejected until read-back confirms ResourceState::Absent.
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(vec!["10.0.0.10/24"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);

        controller.start_reconciliation(&worker);
        let startup_report = worker.recv_report().await.context("startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        let acquire_report = worker.recv_report().await.context("acquire report")?;
        controller.apply_worker_report(acquire_report, &worker);
        assert!(controller.owned());

        // Inject delete failure so withdrawal cannot confirm absence.
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.fail_del_addresses.push("10.0.0.10/24".to_owned());
        }
        controller.apply_role_decision(
            HaState::Standby,
            HaDecisionReason::PeerHigherPriority,
            &worker,
        );
        let failed_withdraw = worker
            .recv_report()
            .await
            .context("failed withdraw report")?;
        controller.apply_worker_report(failed_withdraw, &worker);
        assert!(!controller.owned());
        assert!(!controller.can_release_handoff());
        assert!(matches!(
            controller.resource_state(),
            ResourceState::Uncertain {
                last_observation: ObservedPresence::Present,
                ..
            }
        ));

        // Attempt to re-promote while withdrawal is still unconfirmed: rejected by sticky revocation!
        SimulatedKernel::remove_address(&kernel, "eth0", "10.0.0.10/24");
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        assert_eq!(controller.authority(), AuthorityState::Pending);
        let rejected_report = worker
            .recv_report()
            .await
            .context("rejected acquire report")?;
        assert_eq!(
            rejected_report.kind,
            ResourceReportKind::RejectedWhileWithdrawing
        );
        controller.apply_worker_report(rejected_report, &worker);
        assert!(!controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        // Also verify queued Acquire followed by Withdraw drops the queued Acquire.
        worker.set_directive(10, DesiredOwnership::Revoked);
        worker.send_command(ResourceCommand::Acquire(9));
        worker.send_command(ResourceCommand::Withdraw(10));
        let coalesced_withdraw = worker
            .recv_report()
            .await
            .context("coalesced withdraw report")?;
        assert_eq!(coalesced_withdraw.kind, ResourceReportKind::Withdrawn);
        assert_eq!(coalesced_withdraw.resource_state, ResourceState::Absent);
        controller.apply_worker_report(coalesced_withdraw, &worker);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );

        // Now that absence is confirmed, a new election to Active acquires cleanly.
        controller.apply_role_decision(
            HaState::Standby,
            HaDecisionReason::PeerHigherPriority,
            &worker,
        );
        let standby_withdraw = worker.recv_report().await.context("standby withdraw")?;
        controller.apply_worker_report(standby_withdraw, &worker);

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        let reacquired = worker.recv_report().await.context("reacquired report")?;
        controller.apply_worker_report(reacquired, &worker);
        assert!(controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn unmanaged_resource_never_runs_a_command() -> Result<()> {
        // Spec §1, §2 & IS-9: With addresses: [], resource_state is Unmanaged and no command ever runs.
        let kernel = SimulatedKernel::new_shared();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.fail_all_commands = true;
        }
        let manager = managed_address_manager(Vec::new());
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Unmanaged);
        let mut hooks = Vec::new();

        controller.start_reconciliation(&worker);
        let startup_report = worker.recv_report().await.context("startup report")?;
        controller.apply_worker_report(startup_report, &worker);
        assert_eq!(controller.resource_state(), &ResourceState::Unmanaged);
        assert!(!controller.owned());

        record_role_effect(
            controller.apply_role_decision(
                HaState::Active,
                HaDecisionReason::LocalHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        assert_eq!(controller.resource_state(), &ResourceState::Unmanaged);
        assert!(controller.owned());
        assert_eq!(hooks, vec!["promote:local_higher_priority".to_owned()]);

        controller.request_reconcile(&worker);
        let reconcile_report = worker.recv_report().await.context("reconcile report")?;
        controller.apply_worker_report(reconcile_report, &worker);
        assert_eq!(controller.resource_state(), &ResourceState::Unmanaged);
        assert!(controller.owned());

        record_role_effect(
            controller.apply_role_decision(
                HaState::Standby,
                HaDecisionReason::PeerHigherPriority,
                &worker,
            ),
            &mut hooks,
        );
        assert!(!controller.owned());
        assert_eq!(controller.resource_state(), &ResourceState::Unmanaged);
        assert_eq!(
            hooks,
            vec![
                "promote:local_higher_priority".to_owned(),
                "demote:peer_higher_priority".to_owned()
            ]
        );

        let shutdown_report = controller.shutdown(&mut worker).await;
        assert_eq!(shutdown_report.resource_state, ResourceState::Unmanaged);
        assert_eq!(
            SimulatedKernel::calls_snapshot(&kernel),
            Vec::<Vec<String>>::new()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_hook_does_not_delay_withdrawal() -> Result<()> {
        // Invariant I17: Hooks hanging cannot delay withdrawal or deadline handling.
        let kernel = SimulatedKernel::new_shared();
        let manager = managed_address_manager(vec!["10.0.0.10/24"]);
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            manager,
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);

        controller.start_reconciliation(&worker);
        let startup_report = worker.recv_report().await.context("startup report")?;
        controller.apply_worker_report(startup_report, &worker);

        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        let acquire_report = worker.recv_report().await.context("acquire report")?;
        controller.apply_worker_report(acquire_report, &worker);
        assert!(controller.owned());

        // Simulate a hanging promote/demote hook future running concurrently with withdrawal.
        let hook_finished = Arc::new(Mutex::new(false));
        let hook_finished_clone = hook_finished.clone();
        let mut hanging_hook = std::pin::pin!(async move {
            std::future::pending::<()>().await;
            let mut guard = hook_finished_clone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = true;
        });

        let demote_started = Instant::now();
        controller.apply_role_decision(HaState::Standby, HaDecisionReason::PeerTimeout, &worker);
        let withdraw_report = tokio::select! {
            report = worker.recv_report() => report.context("withdraw report")?,
            () = &mut hanging_hook => anyhow::bail!("hanging hook should never complete"),
        };
        controller.apply_worker_report(withdraw_report, &worker);

        // Withdrawal completed in 0ms virtual time while the hook is still hanging.
        assert_eq!(demote_started.elapsed(), Duration::ZERO);
        assert_eq!(controller.resource_state(), &ResourceState::Absent);
        assert!(!controller.owned());
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        assert!(
            !*hook_finished
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        );

        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    fn attach_watchdog(
        controller: &mut OwnershipController,
    ) -> crate::gruezi::watchdog::FakeWatchdog {
        use crate::{
            config::{WatchdogConfig, WatchdogMode},
            gruezi::watchdog::{FakeWatchdog, WatchdogDevice, WatchdogFault, WatchdogManager},
        };
        let fake = FakeWatchdog::new(WatchdogFault::None);
        controller.watchdog = WatchdogManager::new(
            WatchdogConfig {
                mode: WatchdogMode::Required,
                ..WatchdogConfig::default()
            },
            WatchdogDevice::Fake(fake.clone()),
            Duration::from_secs(11),
        );
        fake
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_required_owned_needs_an_armed_device() -> Result<()> {
        let (_, mut worker, mut controller, _) = boot_managed_pair(vec!["10.0.0.10/24"]).await?;
        let _ = attach_watchdog(&mut controller);
        controller.elected_role = HaState::Active;
        controller.authority = AuthorityState::Valid {
            source: AuthoritySource::Peer,
            term: 1,
            expiry: None,
        };
        controller.generation = 1;
        controller.resource_state = ResourceState::Present { generation: 1 };
        assert!(
            !controller.owned(),
            "required watchdog participates in the owned formula"
        );
        controller.watchdog.arm(1, true)?;
        assert!(controller.owned());
        let status = controller.watchdog_status(Instant::now());
        assert_eq!(status.mode, "required");
        assert!(status.armed);
        assert_eq!(status.effective_timeout_ms, Some(15_000));
        assert_eq!(status.last_feed_ago_ms, Some(0));
        assert!(!status.unprotected);
        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_i11_feed_state_enumeration() -> Result<()> {
        use crate::gruezi::watchdog::WatchdogCall;
        let (_, mut worker, mut controller, _) = boot_managed_pair(vec!["10.0.0.10/24"]).await?;
        let fake = attach_watchdog(&mut controller);
        controller.watchdog.arm(7, true)?;
        controller.elected_role = HaState::Active;
        controller.generation = 7;
        let valid = AuthorityState::Valid {
            source: AuthoritySource::Peer,
            term: 1,
            expiry: Some(Instant::now() + Duration::from_secs(60)),
        };
        let authorities = [
            AuthorityState::None,
            AuthorityState::Pending,
            valid,
            AuthorityState::Uncertain {
                source: Some(AuthoritySource::Peer),
                term: 1,
                expiry: None,
            },
            AuthorityState::Valid {
                source: AuthoritySource::Peer,
                term: 1,
                expiry: Some(Instant::now()),
            },
        ];
        let resources = [
            ResourceState::Unmanaged,
            ResourceState::Absent,
            ResourceState::Acquiring { generation: 7 },
            ResourceState::Present { generation: 7 },
            ResourceState::Present { generation: 6 },
            ResourceState::Releasing { generation: 7 },
            ResourceState::Uncertain {
                last_observation: ObservedPresence::Present,
                last_error: None,
            },
        ];
        for authority in authorities {
            for resource in &resources {
                controller.authority = authority;
                controller.resource_state = resource.clone();
                let before = fake.calls().len();
                controller.feed_watchdog(&worker);
                let should_feed = authority == valid
                    && (*resource == ResourceState::Unmanaged
                        || *resource == ResourceState::Present { generation: 7 });
                assert_eq!(
                    fake.calls().len() > before,
                    should_feed,
                    "{authority:?} {resource:?}"
                );
                println!(
                    "WATCHDOG I11 authority={authority:?} resource={resource:?} generation=7 feed={should_feed}"
                );
            }
        }
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|(_, call)| *call == WatchdogCall::KeepAlive)
                .count(),
            2
        );
        println!("WATCHDOG I11 actual device call log: {:?}", fake.calls());
        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_owned_feeds_then_disarms_only_after_withdrawal() -> Result<()> {
        use crate::gruezi::watchdog::WatchdogCall;
        let (kernel, mut worker, mut controller, mut hooks) =
            boot_managed_pair(vec!["10.0.0.10/24"]).await?;
        let fake = attach_watchdog(&mut controller);
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        assert!(!controller.owned());
        controller.feed_watchdog(&worker);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::KeepAlive)
        );
        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        assert!(controller.owned());
        controller.feed_watchdog(&worker);
        println!(
            "WATCHDOG QA OWNED resource={:?} kernel={:?} calls={:?}",
            controller.resource_state(),
            SimulatedKernel::table_snapshot(&kernel),
            fake.calls()
        );
        controller.revoke_authority(HaDecisionReason::GracefulShutdown, &worker);
        let before = fake.calls();
        controller.feed_watchdog(&worker);
        assert_eq!(before, fake.calls());
        assert!(
            !fake
                .calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::MagicClose)
        );
        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        assert!(!controller.watchdog_status(Instant::now()).armed);
        assert!(
            fake.calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::MagicClose)
        );
        println!(
            "WATCHDOG QA ABSENT kernel={:?} calls={:?}",
            SimulatedKernel::table_snapshot(&kernel),
            fake.calls()
        );
        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_i12_failed_withdrawal_never_disarms_or_magic_closes() -> Result<()> {
        use crate::gruezi::watchdog::WatchdogCall;
        let (kernel, mut worker, mut controller, mut hooks) =
            boot_managed_pair(vec!["10.0.0.10/24"]).await?;
        let fake = attach_watchdog(&mut controller);
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        controller.feed_watchdog(&worker);
        kernel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail_del_addresses
            .push("10.0.0.10/24".to_owned());
        controller.revoke_authority(HaDecisionReason::GracefulShutdown, &worker);
        let before = fake.calls();
        let report = recv_and_apply_report(&mut worker, &mut controller, &mut hooks).await?;
        assert_eq!(report.kind, ResourceReportKind::Fault);
        controller.feed_watchdog(&worker);
        assert_eq!(
            before,
            fake.calls(),
            "withdrawal failure must issue no device calls"
        );
        assert!(controller.watchdog_status(Instant::now()).armed);
        assert!(!controller.owned());
        let deadline = fake.lock().reset_deadline.context("armed deadline")?;
        tokio::time::advance(deadline.saturating_duration_since(Instant::now())).await;
        let calls = fake.calls();
        assert!(
            calls
                .iter()
                .any(|(_, call)| *call == WatchdogCall::Reset { deadline })
        );
        assert!(!calls.iter().any(|(_, call)| matches!(
            call,
            WatchdogCall::Disarm | WatchdogCall::MagicClose | WatchdogCall::Close
        )));
        println!(
            "WATCHDOG QA FAILED WITHDRAWAL resource={:?} kernel={:?} reset_deadline={deadline:?} calls={calls:?}",
            controller.resource_state(),
            SimulatedKernel::table_snapshot(&kernel)
        );
        let shutdown = controller.shutdown(&mut worker).await;
        assert_eq!(shutdown.kind, ResourceReportKind::Fault);
        assert_eq!(
            calls,
            fake.calls(),
            "failed shutdown must also leave the watchdog unfed"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_i16_restart_reconciles_before_any_open() -> Result<()> {
        use crate::gruezi::watchdog::WatchdogCall;
        let kernel = SimulatedKernel::new_shared();
        SimulatedKernel::insert_address(&kernel, "eth0", "10.0.0.10/24", "");
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            managed_address_manager(vec!["10.0.0.10/24"]),
            CommandRunner::Simulated(kernel.clone()),
        );
        let mut controller = OwnershipController::new(ResourceManagement::Managed);
        let fake = attach_watchdog(&mut controller);
        controller.start_reconciliation(&worker);
        controller.apply_role_decision(
            HaState::Active,
            HaDecisionReason::LocalHigherPriority,
            &worker,
        );
        assert_eq!(controller.authority(), AuthorityState::Pending);
        assert!(
            fake.calls().is_empty(),
            "no watchdog open before reconciliation completes"
        );
        let report = worker.recv_report().await.context("startup withdrawal")?;
        assert_eq!(report.resource_state, ResourceState::Absent);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        assert_eq!(fake.calls(), Vec::new());
        controller.apply_worker_report(report, &worker);
        assert!(
            fake.calls()
                .iter()
                .any(|(_, call)| *call == WatchdogCall::Open)
        );
        println!(
            "WATCHDOG I16 residual VIP removed BEFORE first open: {:?}",
            fake.calls()
        );
        let acquired = worker.recv_report().await.context("acquisition")?;
        controller.apply_worker_report(acquired, &worker);
        assert!(controller.owned());
        let _ = controller.shutdown(&mut worker).await;
        Ok(())
    }
}
