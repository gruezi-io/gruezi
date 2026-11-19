//! Coordinator-owned release-then-acquire state. Only fresh session adverts
//! enter this module; retransmission is the ordinary periodic advert path.

use super::super::{
    config::HaRuntimeConfig,
    election::{HaDecision, HaDecisionReason, HaState, PeerObservation, outranks},
    protocol::{AdvertAuthoritySource, AdvertResourceState, HaPacket, Nonce, TransferPhase},
};
use crate::gruezi::ownership::OwnershipController;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default)]
enum Phase {
    #[default]
    Idle,
    Requesting,
    Releasing(u64),
    Released(u64),
    Acquiring(u64),
    Owned(u64),
    Returning(u64),
    Returned(u64),
    Reclaiming,
}

#[derive(Debug, Default)]
pub(in crate::gruezi::ha) struct Handoff {
    phase: Phase,
    peer_nonce: Option<Nonce>,
    incoming: Option<(Nonce, u64)>,
    stable_owner_since: Option<Instant>,
    release_requested: bool,
    pub(super) acknowledged: bool,
    peer_phase: Option<TransferPhase>,
}

impl Handoff {
    /// A transfer id is unique within the origin's boot nonce because the
    /// sender sequence never wraps and at least one advert separates transfers.
    pub(super) fn release(&mut self, next_seq: u64) {
        self.phase = Phase::Releasing(next_seq);
        self.release_requested = false;
        self.acknowledged = false;
    }

    pub(super) fn observe(
        &mut self,
        runtime: &HaRuntimeConfig,
        packet: &HaPacket,
        previous_seen: Option<Instant>,
        now: Instant,
        ownership: &OwnershipController,
    ) {
        let same_session = self.peer_nonce == Some(packet.sender_nonce);
        if self.peer_nonce.is_some() && !same_session {
            // A restarted origin has lost its transfer state. Strict drains
            // the old grant before either side can bootstrap (I4). Available
            // retains its explicit assumed fallback and incumbent behavior.
            if runtime.failure_policy == crate::config::FailurePolicy::Strict {
                self.fault();
            }
            if matches!(self.phase, Phase::Reclaiming) {
                self.phase = Phase::Idle;
            }
        }
        let continuous =
            same_session && previous_seen.is_some_and(|seen| seen + runtime.dead_timeout() > now);
        self.peer_nonce = Some(packet.sender_nonce);
        self.peer_phase = Some(packet.transfer_phase);
        if packet.role == HaState::Active && packet.authority_source != AdvertAuthoritySource::None
        {
            if !continuous || self.stable_owner_since.is_none() {
                self.stable_owner_since = Some(now);
            }
        } else {
            self.stable_owner_since = None;
        }

        let peer_absent = packet.authority_source == AdvertAuthoritySource::None
            && matches!(
                packet.resource_state,
                AdvertResourceState::Absent | AdvertResourceState::Unmanaged
            );
        // A release proof only speaks for the incarnation that released. A
        // new nonce (restart) or a peer that again claims authority or a
        // possibly held resource may own the VIP, so the unused proof is
        // discarded; acquisition then needs fresh evidence (bootstrap,
        // consent, grant plus fencing). Authority already granted from the
        // proof is ownership the peer saw and is kept.
        if matches!(self.phase, Phase::Acquiring(_))
            && !ownership.authority().is_valid()
            && (!peer_absent
                || self
                    .incoming
                    .is_none_or(|(nonce, _)| nonce != packet.sender_nonce))
        {
            self.phase = Phase::Idle;
        }
        if let Phase::Released(id) = self.phase {
            if packet.transfer_id == id {
                if packet.transfer_phase == TransferPhase::Returned && peer_absent {
                    self.phase = Phase::Reclaiming;
                    self.acknowledged = true;
                } else if packet.transfer_phase == TransferPhase::Owned
                    && packet.authority_source != AdvertAuthoritySource::None
                    && matches!(
                        packet.resource_state,
                        AdvertResourceState::Present | AdvertResourceState::Unmanaged
                    )
                {
                    // Keep Released forever: even a lost subsequent Owned or
                    // consent expiry never permits the origin to reacquire.
                    self.acknowledged = true;
                }
            }
            return;
        }
        if packet.transfer_phase == TransferPhase::Requesting && ownership.authority().is_valid() {
            self.release_requested = true;
        }
        let token = (packet.sender_nonce, packet.transfer_id);
        if packet.transfer_phase == TransferPhase::Released
            && packet.transfer_id != 0
            && peer_absent
            && self.incoming != Some(token)
            && ownership.can_release_handoff()
            && !matches!(self.phase, Phase::Releasing(_) | Phase::Returning(_))
        {
            self.incoming = Some(token);
            self.phase = Phase::Acquiring(packet.transfer_id);
        }
    }

    pub(super) fn fault(&mut self) {
        if let Phase::Acquiring(id) | Phase::Owned(id) = self.phase {
            self.phase = Phase::Returning(id);
        }
    }

    pub(super) fn advance(&mut self, ownership: &OwnershipController, next_seq: u64) {
        if self.release_requested && ownership.authority().is_valid() {
            self.release(next_seq);
        }
        match self.phase {
            Phase::Releasing(id) if ownership.can_release_handoff() => {
                self.phase = Phase::Released(id);
            }
            Phase::Returning(id) if ownership.can_release_handoff() => {
                self.phase = Phase::Returned(id);
            }
            Phase::Acquiring(id) if ownership.owned() => self.phase = Phase::Owned(id),
            Phase::Reclaiming if ownership.owned() => self.phase = Phase::Idle,
            _ => {}
        }
    }

    pub(super) fn decision(
        &mut self,
        runtime: &HaRuntimeConfig,
        peer: Option<&PeerObservation>,
        now: Instant,
        local_authorized: bool,
        mut decision: HaDecision,
    ) -> HaDecision {
        let incumbent = peer.filter(|peer| {
            peer.observed_at + runtime.dead_timeout() > now
                && peer.state == HaState::Active
                && peer.authority_source != AdvertAuthoritySource::None
        });
        if matches!(self.phase, Phase::Idle | Phase::Requesting) && !local_authorized {
            if let Some(peer) = incumbent {
                // Preemption is a request after continuous fresh-peer
                // hold-down. Rank alone cannot authorize this node.
                if runtime.preempt
                    && outranks(
                        runtime.priority,
                        &runtime.node_id,
                        peer.priority,
                        &peer.node_id,
                    )
                    && self.stable_owner_since.is_some_and(|since| {
                        since + Duration::from_millis(runtime.hold_down_ms) <= now
                    })
                {
                    self.phase = Phase::Requesting;
                }
                decision.state = HaState::Standby;
            } else if matches!(self.phase, Phase::Requesting) {
                self.phase = Phase::Idle;
            }
        }
        match self.phase {
            Phase::Releasing(_) | Phase::Released(_) | Phase::Returning(_) | Phase::Returned(_) => {
                decision.state = HaState::Standby;
                decision.reason = if matches!(self.phase, Phase::Returning(_)) {
                    HaDecisionReason::AddressActionFailed
                } else {
                    peer.map_or(HaDecisionReason::GracefulShutdown, |peer| {
                        if peer.priority > runtime.priority {
                            HaDecisionReason::PeerHigherPriority
                        } else {
                            HaDecisionReason::PeerNodeIdTiebreak
                        }
                    })
                };
            }
            Phase::Acquiring(_) => {
                decision.state = HaState::Active;
                decision.reason = peer.map_or(HaDecisionReason::PeerReleased, |peer| {
                    if runtime.preempt
                        && outranks(
                            runtime.priority,
                            &runtime.node_id,
                            peer.priority,
                            &peer.node_id,
                        )
                    {
                        if runtime.priority > peer.priority {
                            HaDecisionReason::PreemptHigherPriority
                        } else {
                            HaDecisionReason::PreemptNodeIdTiebreak
                        }
                    } else {
                        HaDecisionReason::PeerReleased
                    }
                });
            }
            Phase::Owned(_) => decision.state = HaState::Active,
            Phase::Reclaiming => {
                decision.state = HaState::Active;
                decision.reason = HaDecisionReason::PeerReturned;
            }
            Phase::Idle | Phase::Requesting => {}
        }
        decision
    }

    pub(super) const fn allowed(&self) -> bool {
        matches!(
            self.phase,
            Phase::Acquiring(_) | Phase::Owned(_) | Phase::Reclaiming
        )
    }

    pub(super) const fn blocked(&self) -> bool {
        matches!(
            self.phase,
            Phase::Requesting
                | Phase::Releasing(_)
                | Phase::Released(_)
                | Phase::Returning(_)
                | Phase::Returned(_)
        )
    }

    pub(super) const fn bootstrap_permitted(&self) -> bool {
        matches!(self.phase, Phase::Idle)
            && matches!(
                self.peer_phase,
                None | Some(TransferPhase::None | TransferPhase::Returned)
            )
    }

    pub(in crate::gruezi::ha) const fn advert(&self) -> (u64, TransferPhase) {
        match self.phase {
            Phase::Requesting => (0, TransferPhase::Requesting),
            Phase::Released(id) => (id, TransferPhase::Released),
            Phase::Owned(id) => (id, TransferPhase::Owned),
            Phase::Returned(id) => (id, TransferPhase::Returned),
            Phase::Idle
            | Phase::Releasing(_)
            | Phase::Acquiring(_)
            | Phase::Returning(_)
            | Phase::Reclaiming => (0, TransferPhase::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Handoff, Instant};
    use crate::{
        config::Config,
        gruezi::{
            ha::{
                HaRuntimeConfig,
                protocol::{
                    AdvertAuthoritySource, AdvertResourceState, TransferPhase, tests::sample_packet,
                },
            },
            ownership::{OwnershipController, ResourceManagement},
        },
    };
    use anyhow::Result;

    fn runtime() -> Result<HaRuntimeConfig> {
        runtime_with_policy("strict")
    }

    fn runtime_with_policy(policy: &str) -> Result<HaRuntimeConfig> {
        HaRuntimeConfig::try_from(&Config::from_yaml_str(&format!(
            "mode: ha\nnode: {{id: node-a}}\nha:\n  failure_policy: {policy}\n  group_id: cluster-ha\n  peer_id: node-b\n  peer: 127.0.0.1:9\n  addresses: []\n  auth: {{key: shared-secret-for-tests}}\n",
        ))?)
    }

    #[test]
    fn release_proof_is_discarded_once_the_releasing_peer_restarts_or_reclaims() -> Result<()> {
        let runtime = runtime_with_policy("available")?;
        let ownership = OwnershipController::new(ResourceManagement::Unmanaged);
        let mut released = sample_packet();
        released.transfer_id = 41;
        released.transfer_phase = TransferPhase::Released;
        released.authority_source = AdvertAuthoritySource::None;
        released.resource_state = AdvertResourceState::Absent;
        let restarted = super::HaPacket {
            sender_nonce: [0x42; 16],
            transfer_id: 0,
            transfer_phase: TransferPhase::None,
            ..released.clone()
        };
        let reclaimed = super::HaPacket {
            transfer_phase: TransferPhase::None,
            authority_source: AdvertAuthoritySource::Peer,
            resource_state: AdvertResourceState::Present,
            ..released.clone()
        };
        for later in [restarted, reclaimed] {
            let mut handoff = Handoff::default();
            handoff.observe(&runtime, &released, None, Instant::now(), &ownership);
            handoff.observe(&runtime, &released, None, Instant::now(), &ownership);
            assert!(handoff.allowed(), "a repeated release keeps the proof");
            handoff.observe(&runtime, &later, None, Instant::now(), &ownership);
            assert!(!handoff.allowed(), "stale proof survived: {later:?}");
            handoff.observe(&runtime, &released, None, Instant::now(), &ownership);
            assert!(
                !handoff.allowed(),
                "the consumed release token cannot be reused"
            );
        }
        Ok(())
    }

    #[test]
    fn invalid_release_evidence_never_authorizes_acquisition() -> Result<()> {
        let runtime = runtime()?;
        let ownership = OwnershipController::new(ResourceManagement::Unmanaged);
        let mut packet = sample_packet();
        packet.transfer_id = 41;
        packet.transfer_phase = TransferPhase::Released;
        packet.authority_source = AdvertAuthoritySource::None;
        packet.resource_state = AdvertResourceState::Absent;
        for (id, source, resource) in [
            (0, AdvertAuthoritySource::None, AdvertResourceState::Absent),
            (41, AdvertAuthoritySource::Peer, AdvertResourceState::Absent),
            (
                41,
                AdvertAuthoritySource::None,
                AdvertResourceState::Present,
            ),
            (
                41,
                AdvertAuthoritySource::None,
                AdvertResourceState::Uncertain,
            ),
        ] {
            let mut handoff = Handoff::default();
            let invalid = super::HaPacket {
                transfer_id: id,
                authority_source: source,
                resource_state: resource,
                ..packet.clone()
            };
            handoff.observe(&runtime, &invalid, None, Instant::now(), &ownership);
            assert!(!handoff.allowed());
            assert!(!handoff.bootstrap_permitted());
        }
        let mut handoff = Handoff::default();
        handoff.observe(&runtime, &packet, None, Instant::now(), &ownership);
        assert!(handoff.allowed());
        assert_eq!(
            handoff.advert(),
            (0, TransferPhase::None),
            "presence is still unconfirmed"
        );
        Ok(())
    }

    #[test]
    fn released_origin_requires_matching_absent_return_even_after_owned() -> Result<()> {
        let runtime = runtime()?;
        let ownership = OwnershipController::new(ResourceManagement::Unmanaged);
        let mut handoff = Handoff::default();
        handoff.release(41);
        handoff.advance(&ownership, 42);
        assert_eq!(handoff.advert(), (41, TransferPhase::Released));
        let mut packet = sample_packet();
        packet.transfer_id = 41;
        packet.transfer_phase = TransferPhase::Owned;
        packet.authority_source = AdvertAuthoritySource::Peer;
        packet.resource_state = AdvertResourceState::Present;
        handoff.observe(&runtime, &packet, None, Instant::now(), &ownership);
        assert!(handoff.acknowledged);
        assert!(handoff.blocked());
        assert!(!handoff.allowed());
        packet.transfer_phase = TransferPhase::Returned;
        packet.authority_source = AdvertAuthoritySource::None;
        packet.transfer_id = 42;
        packet.resource_state = AdvertResourceState::Absent;
        handoff.observe(&runtime, &packet, None, Instant::now(), &ownership);
        assert!(
            handoff.blocked(),
            "a different transfer cannot return this release"
        );
        packet.transfer_id = 41;
        packet.resource_state = AdvertResourceState::Uncertain;
        handoff.observe(&runtime, &packet, None, Instant::now(), &ownership);
        assert!(handoff.blocked(), "return must confirm absence");
        packet.resource_state = AdvertResourceState::Absent;
        handoff.observe(&runtime, &packet, None, Instant::now(), &ownership);
        assert!(handoff.allowed());
        assert!(!handoff.blocked());
        Ok(())
    }
}
