use super::config::HaRuntimeConfig;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HaState {
    Init,
    Standby,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaDecisionReason {
    Startup,
    StartupHold,
    StartupDeadlineExpired,
    AlreadyActive,
    PeerTimeout,
    LocalHigherPriority,
    LocalNodeIdTiebreak,
    PeerHigherPriority,
    PeerNodeIdTiebreak,
    PeerActiveNoPreempt,
    PeerBecameActiveConflict,
    PreemptHigherPriority,
    PreemptNodeIdTiebreak,
    GracefulShutdown,
    RuntimeFault,
    AddressActionFailed,
}

impl HaDecisionReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::StartupHold => "startup_hold",
            Self::StartupDeadlineExpired => "startup_deadline_expired",
            Self::AlreadyActive => "already_active",
            Self::PeerTimeout => "peer_timeout",
            Self::LocalHigherPriority => "local_higher_priority",
            Self::LocalNodeIdTiebreak => "local_node_id_tiebreak",
            Self::PeerHigherPriority => "peer_higher_priority",
            Self::PeerNodeIdTiebreak => "peer_node_id_tiebreak",
            Self::PeerActiveNoPreempt => "peer_active_no_preempt",
            Self::PeerBecameActiveConflict => "peer_became_active_conflict",
            Self::PreemptHigherPriority => "preempt_higher_priority",
            Self::PreemptNodeIdTiebreak => "preempt_node_id_tiebreak",
            Self::GracefulShutdown => "graceful_shutdown",
            Self::RuntimeFault => "runtime_fault",
            Self::AddressActionFailed => "address_action_failed",
        }
    }
}

impl std::fmt::Display for HaDecisionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl HaState {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Init => 0,
            Self::Standby => 1,
            Self::Active => 2,
        }
    }
}

impl TryFrom<u8> for HaState {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Init),
            1 => Ok(Self::Standby),
            2 => Ok(Self::Active),
            _ => bail!("invalid HA state value {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PeerObservation {
    pub(super) node_id: String,
    pub(super) state: HaState,
    pub(super) priority: u8,
    // Keep this sticky for the current peer liveness window so a node that
    // just yielded does not immediately preempt back on the next packet.
    pub(super) became_active: bool,
    pub(super) observed_at: Instant,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HaDecision {
    pub(super) state: HaState,
    pub(super) reason: HaDecisionReason,
}

pub(super) fn desired_state(
    runtime: &HaRuntimeConfig,
    current_state: HaState,
    peer_observation: Option<&PeerObservation>,
    startup_at: Instant,
    now: Instant,
    active_peer_conflict: bool,
) -> HaDecision {
    let startup_deadline = runtime.follower_deadline(startup_at);
    let live_peer =
        peer_observation.filter(|peer| runtime.follower_deadline(peer.observed_at) > now);

    let Some(peer) = live_peer else {
        return desired_state_without_live_peer(
            current_state,
            peer_observation,
            startup_deadline,
            now,
        );
    };

    match peer.state {
        HaState::Active => desired_state_with_active_peer(runtime, peer, active_peer_conflict),
        HaState::Init | HaState::Standby => {
            if current_state == HaState::Active {
                HaDecision {
                    state: HaState::Active,
                    reason: HaDecisionReason::AlreadyActive,
                }
            } else if outranks(
                runtime.priority,
                &runtime.node_id,
                peer.priority,
                &peer.node_id,
            ) {
                HaDecision {
                    state: HaState::Active,
                    reason: local_win_reason(
                        runtime.priority,
                        peer.priority,
                        HaDecisionReason::LocalHigherPriority,
                        HaDecisionReason::LocalNodeIdTiebreak,
                    ),
                }
            } else {
                HaDecision {
                    state: HaState::Standby,
                    reason: peer_win_reason(
                        runtime.priority,
                        peer.priority,
                        HaDecisionReason::PeerHigherPriority,
                        HaDecisionReason::PeerNodeIdTiebreak,
                    ),
                }
            }
        }
    }
}

pub(super) fn has_live_active_peer(
    runtime: &HaRuntimeConfig,
    peer_observation: Option<&PeerObservation>,
    now: Instant,
) -> bool {
    peer_observation
        .filter(|peer| runtime.follower_deadline(peer.observed_at) > now)
        .is_some_and(|peer| peer.state == HaState::Active)
}

fn desired_state_without_live_peer(
    current_state: HaState,
    peer_observation: Option<&PeerObservation>,
    startup_deadline: Instant,
    now: Instant,
) -> HaDecision {
    if peer_observation.is_some() {
        HaDecision {
            state: HaState::Active,
            reason: HaDecisionReason::PeerTimeout,
        }
    } else if current_state == HaState::Active {
        HaDecision {
            state: HaState::Active,
            reason: HaDecisionReason::AlreadyActive,
        }
    } else if now >= startup_deadline {
        HaDecision {
            state: HaState::Active,
            reason: HaDecisionReason::StartupDeadlineExpired,
        }
    } else {
        HaDecision {
            state: HaState::Standby,
            reason: HaDecisionReason::StartupHold,
        }
    }
}

fn desired_state_with_active_peer(
    runtime: &HaRuntimeConfig,
    peer: &PeerObservation,
    active_peer_conflict: bool,
) -> HaDecision {
    let local_wins = outranks(
        runtime.priority,
        &runtime.node_id,
        peer.priority,
        &peer.node_id,
    );

    if local_wins && (peer.became_active || active_peer_conflict) {
        return HaDecision {
            state: HaState::Standby,
            reason: HaDecisionReason::PeerBecameActiveConflict,
        };
    }

    if runtime.preempt && local_wins {
        HaDecision {
            state: HaState::Active,
            reason: local_win_reason(
                runtime.priority,
                peer.priority,
                HaDecisionReason::PreemptHigherPriority,
                HaDecisionReason::PreemptNodeIdTiebreak,
            ),
        }
    } else if runtime.preempt {
        HaDecision {
            state: HaState::Standby,
            reason: peer_win_reason(
                runtime.priority,
                peer.priority,
                HaDecisionReason::PeerHigherPriority,
                HaDecisionReason::PeerNodeIdTiebreak,
            ),
        }
    } else {
        HaDecision {
            state: HaState::Standby,
            reason: HaDecisionReason::PeerActiveNoPreempt,
        }
    }
}

fn outranks(
    local_priority: u8,
    local_node_id: &str,
    peer_priority: u8,
    peer_node_id: &str,
) -> bool {
    rank_order(local_priority, local_node_id, peer_priority, peer_node_id) == Ordering::Greater
}

fn rank_order(
    local_priority: u8,
    local_node_id: &str,
    peer_priority: u8,
    peer_node_id: &str,
) -> Ordering {
    match local_priority.cmp(&peer_priority) {
        Ordering::Equal => local_node_id.cmp(peer_node_id),
        ordering => ordering,
    }
}

fn local_win_reason(
    local_priority: u8,
    peer_priority: u8,
    higher_priority_reason: HaDecisionReason,
    tiebreak_reason: HaDecisionReason,
) -> HaDecisionReason {
    if local_priority > peer_priority {
        higher_priority_reason
    } else {
        tiebreak_reason
    }
}

fn peer_win_reason(
    local_priority: u8,
    peer_priority: u8,
    higher_priority_reason: HaDecisionReason,
    tiebreak_reason: HaDecisionReason,
) -> HaDecisionReason {
    if local_priority < peer_priority {
        higher_priority_reason
    } else {
        tiebreak_reason
    }
}

#[cfg(test)]
mod tests {
    use super::{HaDecisionReason, HaState, PeerObservation, desired_state};
    use crate::{config::Config, gruezi::ha::HaRuntimeConfig};
    use anyhow::Result;
    use std::time::{Duration, Instant};

    fn runtime_with_timers(
        advert_interval_ms: u64,
        dead_factor: u8,
        hold_down_ms: u64,
    ) -> Result<HaRuntimeConfig> {
        Config::from_yaml_str(&format!(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: {advert_interval_ms}
  dead_factor: {dead_factor}
  hold_down_ms: {hold_down_ms}
  jitter_ms: 0
  auth:
    mode: none
"
        ))
        .and_then(|config| HaRuntimeConfig::try_from(&config))
    }

    #[test]
    fn higher_priority_standby_promotes_when_it_outranks_peer() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-b
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 110
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-a".to_owned(),
            state: HaState::Standby,
            priority: 100,
            became_active: false,
            observed_at: Instant::now(),
        };

        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Active);
        assert_eq!(decision.reason, HaDecisionReason::LocalHigherPriority);

        Ok(())
    }

    #[test]
    fn lower_priority_node_stays_standby_when_peer_is_active() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  preempt: true
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Active,
            priority: 110,
            became_active: false,
            observed_at: Instant::now(),
        };

        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Standby);
        assert_eq!(decision.reason, HaDecisionReason::PeerHigherPriority);

        Ok(())
    }

    #[test]
    fn startup_is_conservative_before_deadline() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let startup_at = Instant::now();

        let hold_decision = desired_state(
            &runtime,
            HaState::Init,
            None,
            startup_at,
            startup_at + Duration::from_millis(500),
            false,
        );
        let expired_decision = desired_state(
            &runtime,
            HaState::Init,
            None,
            startup_at,
            startup_at + runtime.dead_timeout() + runtime.hold_down(),
            false,
        );

        assert_eq!(hold_decision.state, HaState::Standby);
        assert_eq!(hold_decision.reason, HaDecisionReason::StartupHold);
        assert_eq!(expired_decision.state, HaState::Active);
        assert_eq!(
            expired_decision.reason,
            HaDecisionReason::StartupDeadlineExpired
        );

        Ok(())
    }

    #[test]
    fn startup_deadline_respects_multiple_timer_combinations() -> Result<()> {
        let timer_cases = [
            (200_u64, 2_u8, 100_u64),
            (500_u64, 3_u8, 250_u64),
            (750_u64, 4_u8, 0_u64),
        ];

        for (advert_interval_ms, dead_factor, hold_down_ms) in timer_cases {
            let runtime = runtime_with_timers(advert_interval_ms, dead_factor, hold_down_ms)?;
            let startup_at = Instant::now();
            let deadline = runtime.dead_timeout() + runtime.hold_down();

            let before_deadline = desired_state(
                &runtime,
                HaState::Init,
                None,
                startup_at,
                startup_at + deadline.saturating_sub(Duration::from_millis(1)),
                false,
            );
            let at_deadline = desired_state(
                &runtime,
                HaState::Init,
                None,
                startup_at,
                startup_at + deadline,
                false,
            );

            assert_eq!(before_deadline.state, HaState::Standby);
            assert_eq!(before_deadline.reason, HaDecisionReason::StartupHold);
            assert_eq!(at_deadline.state, HaState::Active);
            assert_eq!(at_deadline.reason, HaDecisionReason::StartupDeadlineExpired);
        }

        Ok(())
    }

    #[test]
    fn preempt_false_keeps_higher_priority_peer_in_standby() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 110
  preempt: false
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Active,
            priority: 100,
            became_active: false,
            observed_at: Instant::now(),
        };

        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Standby);
        assert_eq!(decision.reason, HaDecisionReason::PeerActiveNoPreempt);

        Ok(())
    }

    #[test]
    fn peer_timeout_promotes_a_waiting_standby() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let startup_at = Instant::now();
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Active,
            priority: 110,
            became_active: false,
            observed_at: startup_at,
        };
        let now =
            startup_at + runtime.dead_timeout() + runtime.hold_down() + Duration::from_millis(1);
        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            startup_at,
            now,
            false,
        );

        assert_eq!(decision.state, HaState::Active);
        assert_eq!(decision.reason, HaDecisionReason::PeerTimeout);

        Ok(())
    }

    #[test]
    fn peer_timeout_deadline_respects_multiple_timer_combinations() -> Result<()> {
        let timer_cases = [
            (200_u64, 2_u8, 100_u64),
            (400_u64, 3_u8, 200_u64),
            (800_u64, 2_u8, 500_u64),
        ];

        for (advert_interval_ms, dead_factor, hold_down_ms) in timer_cases {
            let runtime = runtime_with_timers(advert_interval_ms, dead_factor, hold_down_ms)?;
            let observed_at = Instant::now();
            let peer = PeerObservation {
                node_id: "node-b".to_owned(),
                state: HaState::Active,
                priority: 110,
                became_active: false,
                observed_at,
            };
            let deadline = runtime.dead_timeout() + runtime.hold_down();

            let before_deadline = desired_state(
                &runtime,
                HaState::Standby,
                Some(&peer),
                observed_at,
                observed_at + deadline.saturating_sub(Duration::from_millis(1)),
                false,
            );
            let after_deadline = desired_state(
                &runtime,
                HaState::Standby,
                Some(&peer),
                observed_at,
                observed_at + deadline + Duration::from_millis(1),
                false,
            );

            assert_eq!(before_deadline.state, HaState::Standby);
            assert!(matches!(
                before_deadline.reason,
                HaDecisionReason::PeerHigherPriority | HaDecisionReason::PeerActiveNoPreempt
            ));
            assert_eq!(after_deadline.state, HaState::Active);
            assert_eq!(after_deadline.reason, HaDecisionReason::PeerTimeout);
        }

        Ok(())
    }

    #[test]
    fn active_node_keeps_ownership_against_standby_peer() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Standby,
            priority: 110,
            became_active: false,
            observed_at: Instant::now(),
        };
        let decision = desired_state(
            &runtime,
            HaState::Active,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Active);
        assert_eq!(decision.reason, HaDecisionReason::AlreadyActive);

        Ok(())
    }

    #[test]
    fn active_node_yields_when_peer_newly_becomes_active() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 110
  preempt: true
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Active,
            priority: 100,
            became_active: true,
            observed_at: Instant::now(),
        };
        let decision = desired_state(
            &runtime,
            HaState::Active,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Standby);
        assert_eq!(decision.reason, HaDecisionReason::PeerBecameActiveConflict);

        Ok(())
    }

    #[test]
    fn standby_node_does_not_immediately_reclaim_after_conflict_yield() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 110
  preempt: true
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-b".to_owned(),
            state: HaState::Active,
            priority: 100,
            became_active: true,
            observed_at: Instant::now(),
        };
        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            true,
        );

        assert_eq!(decision.state, HaState::Standby);
        assert_eq!(decision.reason, HaDecisionReason::PeerBecameActiveConflict);

        Ok(())
    }

    #[test]
    fn equal_priority_preempt_uses_node_id_tiebreak_reason() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-b
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:9375
  protocol_version: 1
  priority: 100
  preempt: true
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: none
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let peer = PeerObservation {
            node_id: "node-a".to_owned(),
            state: HaState::Active,
            priority: 100,
            became_active: false,
            observed_at: Instant::now(),
        };
        let decision = desired_state(
            &runtime,
            HaState::Standby,
            Some(&peer),
            Instant::now(),
            Instant::now(),
            false,
        );

        assert_eq!(decision.state, HaState::Active);
        assert_eq!(decision.reason, HaDecisionReason::PreemptNodeIdTiebreak);

        Ok(())
    }
}
