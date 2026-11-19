//! Peer session tracking: source pinning, identity checks and replay protection.
//!
//! [`PeerTracker`] runs the receive pipeline in the mandated order and is the
//! only owner of session state. Election only ever sees adverts the tracker
//! classified as [`Admission::Fresh`].
//!
//! Session rules (amended design spec 4.3):
//! - every process start draws a fresh random 128-bit nonce; nothing persists;
//! - the recorded peer nonce changes ONLY when an authenticated packet echoes
//!   OUR boot nonce (a replayed packet from an earlier peer incarnation can
//!   never carry our fresh nonce); the previous nonce then moves to the
//!   bounded retired set and the replay window resets;
//! - an authenticated packet with an unknown sender nonce that does NOT echo
//!   our nonce is recorded only in the single-slot candidate so our own
//!   adverts can echo it: it never displaces the recorded peer nonce, never
//!   retires anything and never resets the recorded replay window (this
//!   closes a lockout in which replaying a captured packet of a never-seen
//!   earlier peer incarnation displaced and retired the live peer nonce);
//! - the retired set is bounded; when it is full a further new nonce is
//!   ignored and counted (fail closed) instead of evicting an old entry,
//!   because eviction would let a captured packet of an evicted incarnation
//!   be accepted again;
//! - a packet is FRESH only when it echoes our nonce and carries the recorded
//!   peer nonce with the newest sequence number, so captured packets of an
//!   earlier incarnation of either side can never refresh liveness or
//!   authority; a restarted peer becomes fresh only after echoing our nonce;
//! - one 64-packet sliding window per peer nonce rejects duplicate and
//!   too-old sequence numbers (updated only after HMAC verification); an
//!   unseen reordered sequence number inside the window is recorded but never
//!   refreshes liveness, because only the newest packet proves the peer state
//!   is current. Only the recorded session's window feeds `ack_seq`, so
//!   candidate traffic can never move the acknowledgement.

use super::{
    config::{HaAuth, HaRuntimeConfig},
    election::HaState,
    protocol::{
        AdvertAuthoritySource, AdvertResourceState, HaPacket, Nonce, PacketKey, PacketRejection,
        TransferPhase, ZERO_NONCE,
    },
};
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr},
    time::Instant,
};

/// Width of the per-session sliding replay window.
pub const REPLAY_WINDOW: u64 = 64;
/// Maximum number of retired peer nonces remembered by one process.
pub const RETIRED_NONCE_LIMIT: usize = 64;

/// Rejected datagrams by cause. Counters are the only state a rejection
/// changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRejections {
    pub wrong_source: u64,
    pub malformed: u64,
    pub unsupported: u64,
    pub bad_tag: u64,
    pub wrong_group: u64,
    pub duplicate_node_id: u64,
    pub wrong_sender: u64,
    pub wrong_receiver: u64,
    pub retired_nonce: u64,
    pub nonce_limit: u64,
    pub duplicate: u64,
    pub too_old: u64,
}

impl PeerRejections {
    /// Increment the counter for `cause` and return its new value.
    fn record(&mut self, cause: PacketRejection) -> u64 {
        let counter = match cause {
            PacketRejection::WrongSource => &mut self.wrong_source,
            PacketRejection::Malformed => &mut self.malformed,
            PacketRejection::Unsupported => &mut self.unsupported,
            PacketRejection::BadTag => &mut self.bad_tag,
            PacketRejection::WrongGroup => &mut self.wrong_group,
            PacketRejection::DuplicateNodeId => &mut self.duplicate_node_id,
            PacketRejection::WrongSender => &mut self.wrong_sender,
            PacketRejection::WrongReceiver => &mut self.wrong_receiver,
            PacketRejection::RetiredNonce => &mut self.retired_nonce,
            PacketRejection::NonceLimit => &mut self.nonce_limit,
            PacketRejection::Duplicate => &mut self.duplicate,
            PacketRejection::TooOld => &mut self.too_old,
        };
        *counter = counter.saturating_add(1);
        *counter
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.wrong_source
            .saturating_add(self.malformed)
            .saturating_add(self.unsupported)
            .saturating_add(self.bad_tag)
            .saturating_add(self.wrong_group)
            .saturating_add(self.duplicate_node_id)
            .saturating_add(self.wrong_sender)
            .saturating_add(self.wrong_receiver)
            .saturating_add(self.retired_nonce)
            .saturating_add(self.nonce_limit)
            .saturating_add(self.duplicate)
            .saturating_add(self.too_old)
    }
}

/// Outcome of an accepted (authenticated, not replayed) datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Admission {
    /// Echoes our nonce, carries the recorded peer nonce and is the newest
    /// sequence number (or is the packet that recorded that nonce): the only
    /// outcome that may feed election and liveness.
    Fresh(HaPacket),
    /// Authenticated but not fresh: the packet does not echo our nonce.
    /// `candidate` is true when its sender nonce was recorded in (or already
    /// belongs to) the single-slot establishment candidate instead of the
    /// recorded peer session.
    Unconfirmed { candidate: bool },
    /// Unseen sequence number inside the window but older than the newest:
    /// recorded so a later copy is a duplicate, never refreshes liveness.
    Reordered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeqOrder {
    Newest,
    Reordered,
}

/// 64-entry sliding bitmap. Bit `n` set means `highest - n` was accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReplayWindow {
    highest: u64,
    seen: u64,
}

impl ReplayWindow {
    fn check(&self, seq: u64) -> Result<SeqOrder, PacketRejection> {
        if seq > self.highest {
            return Ok(SeqOrder::Newest);
        }
        let age = self.highest - seq;
        if age >= REPLAY_WINDOW {
            return Err(PacketRejection::TooOld);
        }
        if self.seen & bit(age) != 0 {
            return Err(PacketRejection::Duplicate);
        }
        Ok(SeqOrder::Reordered)
    }

    /// Record an already-checked sequence number.
    fn record(&mut self, seq: u64) {
        if seq > self.highest {
            let shift = seq - self.highest;
            self.seen = u32::try_from(shift)
                .ok()
                .and_then(|shift| self.seen.checked_shl(shift))
                .unwrap_or(0)
                | 1;
            self.highest = seq;
        } else {
            self.seen |= bit(self.highest - seq);
        }
    }
}

fn bit(age: u64) -> u64 {
    u32::try_from(age)
        .ok()
        .and_then(|age| 1_u64.checked_shl(age))
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct PeerSession {
    peer_nonce: Nonce,
    window: ReplayWindow,
    last_fresh_at: Option<Instant>,
}

/// Single-slot establishment candidate (amended design spec 4.3): an unknown
/// peer nonce seen without our nonce echoed yet. It backs only the
/// `receiver_nonce` echo in our own adverts and its own per-nonce replay
/// window; it never feeds liveness and never touches the recorded session.
#[derive(Debug, Clone)]
struct Candidate {
    peer_nonce: Nonce,
    window: ReplayWindow,
}

/// Fields the coordinator contributes to every outgoing advert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AdvertState {
    pub(super) role: HaState,
    pub(super) authority_source: AdvertAuthoritySource,
    pub(super) resource_state: AdvertResourceState,
    pub(super) term: u64,
    pub(super) transfer_id: u64,
    pub(super) transfer_phase: TransferPhase,
}

/// Owns the local nonce and send sequence, the peer session, the single-slot
/// candidate, the retired nonce set and the rejection counters.
#[derive(Debug)]
pub(super) struct PeerTracker {
    key: PacketKey,
    peer_ip: IpAddr,
    group_id: String,
    local_id: String,
    peer_id: String,
    priority: u8,
    local_nonce: Nonce,
    last_sent_seq: u64,
    session: Option<PeerSession>,
    candidate: Option<Candidate>,
    retired: Vec<Nonce>,
    rejections: PeerRejections,
    last_authenticated_at: Option<Instant>,
}

/// Draw the per-process boot nonce from the OS CSPRNG.
///
/// # Errors
///
/// Returns an error if the OS random source fails or yields the all-zero
/// value reserved for "peer nonce not seen yet".
pub(super) fn boot_nonce() -> Result<Nonce> {
    let mut nonce = ZERO_NONCE;
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow!("failed to draw HA boot nonce: {error}"))?;
    if nonce == ZERO_NONCE {
        bail!("OS random source returned an all-zero HA boot nonce");
    }
    Ok(nonce)
}

impl PeerTracker {
    /// `peer_ip` is the configured peer address resolved once at startup.
    pub(super) fn new(
        runtime: &HaRuntimeConfig,
        peer_ip: IpAddr,
        local_nonce: Nonce,
    ) -> Result<Self> {
        let HaAuth::SharedKey { key } = &runtime.auth;
        if local_nonce == ZERO_NONCE {
            bail!("HA local nonce cannot be zero");
        }
        Ok(Self {
            key: PacketKey::new(key.as_bytes())?,
            peer_ip: peer_ip.to_canonical(),
            group_id: runtime.group_id.clone(),
            local_id: runtime.node_id.clone(),
            peer_id: runtime.peer_id.clone(),
            priority: runtime.priority,
            local_nonce,
            last_sent_seq: 0,
            session: None,
            candidate: None,
            retired: Vec::new(),
            rejections: PeerRejections::default(),
            last_authenticated_at: None,
        })
    }

    /// Run the receive pipeline: (1) source IP, (2) framing, (3) HMAC,
    /// (4) group/sender/receiver ids, (5) session and replay. Only an `Ok`
    /// result may change anything beyond the rejection counter.
    pub(super) fn receive(
        &mut self,
        payload: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Result<Admission, (PacketRejection, u64)> {
        self.admit(payload, from, now)
            .map_err(|cause| (cause, self.rejections.record(cause)))
    }

    fn admit(
        &mut self,
        payload: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Result<Admission, PacketRejection> {
        // The socket is dual-stack, so an IPv4 peer arrives as ::ffff:a.b.c.d.
        if from.ip().to_canonical() != self.peer_ip {
            return Err(PacketRejection::WrongSource);
        }
        let packet = HaPacket::decode(payload, &self.key)?;
        if packet.group_id != self.group_id {
            return Err(PacketRejection::WrongGroup);
        }
        if packet.sender_id == self.local_id {
            return Err(PacketRejection::DuplicateNodeId);
        }
        if packet.sender_id != self.peer_id {
            return Err(PacketRejection::WrongSender);
        }
        if packet.receiver_id != self.local_id {
            return Err(PacketRejection::WrongReceiver);
        }
        self.admit_session(packet, now)
    }

    fn admit_session(
        &mut self,
        packet: HaPacket,
        now: Instant,
    ) -> Result<Admission, PacketRejection> {
        if self.retired.contains(&packet.sender_nonce) {
            return Err(PacketRejection::RetiredNonce);
        }
        if let Some(session) = self
            .session
            .as_mut()
            .filter(|session| session.peer_nonce == packet.sender_nonce)
        {
            // Recorded session: the per-nonce replay window runs first, then
            // freshness. Every check above happens before the first mutation
            // so a rejection leaves the session exactly as it was.
            let order = session.window.check(packet.seq)?;
            session.window.record(packet.seq);
            self.last_authenticated_at = Some(now);
            if packet.receiver_nonce != self.local_nonce {
                // Authenticated but stale, e.g. captured packets from before
                // the peer echoed our nonce: never fresh.
                return Ok(Admission::Unconfirmed { candidate: false });
            }
            if order == SeqOrder::Reordered {
                return Ok(Admission::Reordered);
            }
            session.last_fresh_at = Some(now);
            return Ok(Admission::Fresh(packet));
        }

        // The sender nonce is unknown to the recorded session. A new nonce is
        // recorded only in the single-slot candidate, and only while the
        // retired set can still take the previous nonce later (fail closed:
        // when it is full, every further new nonce is ignored and counted).
        let known_candidate = self
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.peer_nonce == packet.sender_nonce);
        if !known_candidate && self.retired.len() >= RETIRED_NONCE_LIMIT {
            return Err(PacketRejection::NonceLimit);
        }

        if packet.receiver_nonce == self.local_nonce {
            // The packet echoes OUR random boot nonce, so it cannot be a
            // replay from an earlier peer incarnation. This is the ONLY path
            // that records a peer nonce (amended design spec 4.3): the
            // previous nonce moves to the retired set and the replay window
            // resets. The packet itself opens the new session and is FRESH,
            // so a restarted peer works exactly as before once it echoes.
            if self.session.is_some() && self.retired.len() >= RETIRED_NONCE_LIMIT {
                return Err(PacketRejection::NonceLimit);
            }
            if let Some(previous) = self.session.take() {
                self.retired.push(previous.peer_nonce);
            }
            self.candidate = None;
            let mut window = ReplayWindow::default();
            window.record(packet.seq);
            self.session = Some(PeerSession {
                peer_nonce: packet.sender_nonce,
                window,
                last_fresh_at: Some(now),
            });
            self.last_authenticated_at = Some(now);
            return Ok(Admission::Fresh(packet));
        }

        // An unknown nonce without our nonce echoed is recorded only in the
        // single-slot candidate so our own adverts can echo it (which is what
        // lets a genuinely restarted peer establish). It never displaces the
        // recorded peer nonce, never retires anything and never resets the
        // recorded replay window.
        if known_candidate {
            if let Some(candidate) = self.candidate.as_mut() {
                candidate.window.check(packet.seq)?;
                candidate.window.record(packet.seq);
            }
        } else {
            let mut window = ReplayWindow::default();
            window.record(packet.seq);
            self.candidate = Some(Candidate {
                peer_nonce: packet.sender_nonce,
                window,
            });
        }
        self.last_authenticated_at = Some(now);
        Ok(Admission::Unconfirmed { candidate: true })
    }

    /// Build and authenticate the next advert. Every call consumes a sequence
    /// number, whether or not the datagram is later delivered.
    ///
    /// # Errors
    ///
    /// Fails closed once the u64 sequence space is exhausted: reusing or
    /// wrapping a sequence number would make our own adverts replayable, so the
    /// runtime must exit and restart with a new nonce.
    pub(super) fn next_advert(&mut self, state: AdvertState) -> Result<(u64, Vec<u8>)> {
        let seq = self
            .last_sent_seq
            .checked_add(1)
            .ok_or_else(|| anyhow!("HA advert sequence space exhausted; restart required"))?;
        self.last_sent_seq = seq;
        let packet = HaPacket {
            group_id: self.group_id.clone(),
            sender_id: self.local_id.clone(),
            receiver_id: self.peer_id.clone(),
            sender_nonce: self.local_nonce,
            // The recorded session wins over the candidate: echoing a
            // candidate while a session is live would break the live peer's
            // freshness check. With no session the candidate is echoed so a
            // genuinely restarted peer can establish.
            receiver_nonce: self.session.as_ref().map_or_else(
                || {
                    self.candidate
                        .as_ref()
                        .map_or(ZERO_NONCE, |candidate| candidate.peer_nonce)
                },
                |session| session.peer_nonce,
            ),
            seq,
            // Only the recorded session is acknowledged: candidate traffic
            // must never move the acknowledgement.
            ack_seq: self
                .session
                .as_ref()
                .map_or(0, |session| session.window.highest),
            role: state.role,
            authority_source: state.authority_source,
            resource_state: state.resource_state,
            priority: self.priority,
            term: state.term,
            transfer_id: state.transfer_id,
            transfer_phase: state.transfer_phase,
        };
        Ok((seq, packet.encode(&self.key)?))
    }

    pub(super) const fn last_sent_seq(&self) -> u64 {
        self.last_sent_seq
    }

    pub(super) const fn rejections(&self) -> PeerRejections {
        self.rejections
    }

    pub(super) const fn last_authenticated_at(&self) -> Option<Instant> {
        self.last_authenticated_at
    }

    /// Time of the newest fresh packet of the recorded peer session; `None`
    /// until a peer nonce is recorded through our-nonce echo.
    pub(super) fn last_fresh_at(&self) -> Option<Instant> {
        self.session
            .as_ref()
            .and_then(|session| session.last_fresh_at)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Admission, AdvertState, PeerRejections, PeerTracker, REPLAY_WINDOW, RETIRED_NONCE_LIMIT,
        ReplayWindow, boot_nonce,
    };
    use crate::{
        config::Config,
        gruezi::ha::{
            HaRuntimeConfig, HaState,
            protocol::{
                AdvertAuthoritySource, AdvertResourceState, HaPacket, Nonce, PacketKey,
                PacketRejection, ZERO_NONCE,
                tests::{TEST_KEY, sample_packet},
            },
        },
    };
    use anyhow::{Result, anyhow};
    use std::{net::SocketAddr, time::Instant};

    const LOCAL_NONCE: Nonce = [0xa5; 16];
    const PEER_NONCE: Nonce = [0xb5; 16];
    const RESTARTED_PEER_NONCE: Nonce = [0xc5; 16];
    /// A peer incarnation this node has never seen: it is neither the
    /// recorded nonce nor in the retired set.
    const EARLIER_INCARNATION_NONCE: Nonce = [0xd5; 16];

    fn runtime() -> Result<HaRuntimeConfig> {
        let config = Config::from_yaml_str(
            "mode: ha\nnode:\n  id: node-a\nha:\n  failure_policy: available\n  peer_id: node-b\n  auth: {key: shared-secret-for-tests}\n  group_id: cluster-ha\n  peer: 127.0.0.1:9\n  addresses: []\n",
        )?;
        HaRuntimeConfig::try_from(&config)
    }

    fn tracker() -> Result<PeerTracker> {
        PeerTracker::new(&runtime()?, "127.0.0.1".parse()?, LOCAL_NONCE)
    }

    fn peer_addr() -> Result<SocketAddr> {
        Ok("127.0.0.1:40000".parse()?)
    }

    /// An advert from node-b (`sample_packet` shape) with the given session
    /// fields, signed with the shared test key.
    fn advert(sender_nonce: Nonce, receiver_nonce: Nonce, seq: u64) -> Result<Vec<u8>> {
        HaPacket {
            sender_nonce,
            receiver_nonce,
            seq,
            ..sample_packet()
        }
        .encode(&PacketKey::new(TEST_KEY)?)
    }

    fn receive(
        tracker: &mut PeerTracker,
        bytes: &[u8],
    ) -> Result<Result<Admission, PacketRejection>> {
        Ok(tracker
            .receive(bytes, peer_addr()?, Instant::now())
            .map_err(|(cause, _)| cause))
    }

    /// Session-relevant view of the tracker used to prove that a rejection
    /// changed nothing but its counter.
    type SessionView = (
        Option<(Nonce, ReplayWindow, Option<Instant>)>,
        Option<(Nonce, ReplayWindow)>,
        Vec<Nonce>,
        Option<Instant>,
        u64,
    );

    fn session_view(tracker: &PeerTracker) -> SessionView {
        (
            tracker
                .session
                .as_ref()
                .map(|session| (session.peer_nonce, session.window, session.last_fresh_at)),
            tracker
                .candidate
                .as_ref()
                .map(|candidate| (candidate.peer_nonce, candidate.window)),
            tracker.retired.clone(),
            tracker.last_authenticated_at,
            tracker.last_sent_seq,
        )
    }

    fn assert_rejected_without_side_effects(
        tracker: &mut PeerTracker,
        bytes: &[u8],
        from: SocketAddr,
        expected: PacketRejection,
    ) {
        let before = session_view(tracker);
        let counters_before = tracker.rejections();
        let result = tracker.receive(bytes, from, Instant::now());
        assert_eq!(result.map_err(|(cause, _)| cause), Err(expected));
        assert_eq!(
            session_view(tracker),
            before,
            "{expected} changed session state"
        );
        assert_eq!(tracker.rejections().total(), counters_before.total() + 1);
    }

    fn fresh_session(tracker: &mut PeerTracker) -> Result<()> {
        assert_eq!(
            receive(tracker, &advert(PEER_NONCE, ZERO_NONCE, 1)?)?,
            Ok(Admission::Unconfirmed { candidate: true })
        );
        assert!(matches!(
            receive(tracker, &advert(PEER_NONCE, LOCAL_NONCE, 2)?)?,
            Ok(Admission::Fresh(_))
        ));
        Ok(())
    }

    #[test]
    fn session_needs_two_adverts_and_an_echo_of_our_nonce() -> Result<()> {
        let mut tracker = tracker()?;
        assert_eq!(
            receive(&mut tracker, &advert(PEER_NONCE, ZERO_NONCE, 1)?)?,
            Ok(Admission::Unconfirmed { candidate: true })
        );
        assert!(tracker.last_fresh_at().is_none());
        // Our next advert echoes the candidate nonce; without a recorded
        // session no peer sequence is acknowledged yet (the ack must never
        // move on candidate traffic).
        let (_, ours) = tracker.next_advert(advert_state())?;
        let ours = HaPacket::decode(&ours, &PacketKey::new(TEST_KEY)?)?;
        assert_eq!(ours.receiver_nonce, PEER_NONCE);
        assert_eq!(ours.ack_seq, 0);
        assert_eq!(ours.sender_nonce, LOCAL_NONCE);

        let fresh = receive(&mut tracker, &advert(PEER_NONCE, LOCAL_NONCE, 2)?)?;
        assert!(matches!(fresh, Ok(Admission::Fresh(ref packet)) if packet.seq == 2));
        assert!(tracker.last_fresh_at().is_some());
        Ok(())
    }

    fn advert_state() -> AdvertState {
        AdvertState {
            role: HaState::Standby,
            authority_source: AdvertAuthoritySource::None,
            resource_state: AdvertResourceState::Unmanaged,
            term: 0,
            transfer_id: 0,
            transfer_phase: super::TransferPhase::None,
        }
    }

    #[test]
    fn rejects_wrong_source_ip_but_accepts_ipv4_mapped_peer() -> Result<()> {
        let mut tracker = tracker()?;
        let bytes = advert(PEER_NONCE, ZERO_NONCE, 1)?;
        assert_rejected_without_side_effects(
            &mut tracker,
            &bytes,
            "127.0.0.2:40000".parse()?,
            PacketRejection::WrongSource,
        );
        assert_rejected_without_side_effects(
            &mut tracker,
            &bytes,
            "[::1]:40000".parse()?,
            PacketRejection::WrongSource,
        );
        // The dual-stack socket reports IPv4 peers as IPv4-mapped IPv6; the
        // port is ignored.
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:1".parse()?;
        assert_eq!(
            tracker
                .receive(&bytes, mapped, Instant::now())
                .map_err(|(cause, _)| cause),
            Ok(Admission::Unconfirmed { candidate: true })
        );
        assert_eq!(tracker.rejections().wrong_source, 2);

        // A configured IPv4-mapped peer address matches a plain IPv4 source.
        let mut mapped_config =
            PeerTracker::new(&runtime()?, "::ffff:127.0.0.1".parse()?, LOCAL_NONCE)?;
        assert!(
            mapped_config
                .receive(&bytes, peer_addr()?, Instant::now())
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn rejects_wrong_group_sender_receiver_and_own_node_id() -> Result<()> {
        let mut tracker = tracker()?;
        fresh_session(&mut tracker)?;
        let key = PacketKey::new(TEST_KEY)?;
        let base = HaPacket {
            sender_nonce: PEER_NONCE,
            receiver_nonce: LOCAL_NONCE,
            seq: 3,
            ..sample_packet()
        };
        let cases = [
            (
                HaPacket {
                    group_id: "other-group".to_owned(),
                    ..base.clone()
                },
                PacketRejection::WrongGroup,
            ),
            (
                HaPacket {
                    sender_id: "node-c".to_owned(),
                    ..base.clone()
                },
                PacketRejection::WrongSender,
            ),
            (
                HaPacket {
                    sender_id: "node-a".to_owned(),
                    ..base.clone()
                },
                PacketRejection::DuplicateNodeId,
            ),
            (
                HaPacket {
                    receiver_id: "node-c".to_owned(),
                    ..base.clone()
                },
                PacketRejection::WrongReceiver,
            ),
        ];
        for (packet, expected) in cases {
            assert_rejected_without_side_effects(
                &mut tracker,
                &packet.encode(&key)?,
                peer_addr()?,
                expected,
            );
        }
        // Seq 3 was never recorded, so the genuine packet is still fresh.
        assert!(matches!(
            receive(&mut tracker, &base.encode(&key)?)?,
            Ok(Admission::Fresh(_))
        ));
        Ok(())
    }

    #[test]
    fn rejects_bad_tag_malformed_and_unsupported_without_state_change() -> Result<()> {
        let mut tracker = tracker()?;
        fresh_session(&mut tracker)?;
        let forged = HaPacket {
            sender_nonce: PEER_NONCE,
            receiver_nonce: LOCAL_NONCE,
            seq: 3,
            ..sample_packet()
        }
        .encode(&PacketKey::new(b"wrong-key-for-forgery")?)?;
        assert_rejected_without_side_effects(
            &mut tracker,
            &forged,
            peer_addr()?,
            PacketRejection::BadTag,
        );
        let genuine = advert(PEER_NONCE, LOCAL_NONCE, 3)?;
        assert_rejected_without_side_effects(
            &mut tracker,
            genuine.get(..20).ok_or_else(|| anyhow!("prefix"))?,
            peer_addr()?,
            PacketRejection::Malformed,
        );
        let mut unknown_kind = genuine.clone();
        if let Some(kind) = unknown_kind.get_mut(5) {
            *kind = 2;
        }
        assert_rejected_without_side_effects(
            &mut tracker,
            &unknown_kind,
            peer_addr()?,
            PacketRejection::Unsupported,
        );
        assert_eq!(
            tracker.rejections(),
            PeerRejections {
                bad_tag: 1,
                malformed: 1,
                unsupported: 1,
                ..PeerRejections::default()
            }
        );
        Ok(())
    }

    #[test]
    fn duplicate_too_old_and_reordered_seq_never_refresh_liveness() -> Result<()> {
        let mut tracker = tracker()?;
        fresh_session(&mut tracker)?;
        let fresh_at = tracker.last_fresh_at();

        // Duplicate of an accepted fresh packet.
        assert_rejected_without_side_effects(
            &mut tracker,
            &advert(PEER_NONCE, LOCAL_NONCE, 2)?,
            peer_addr()?,
            PacketRejection::Duplicate,
        );

        // Jump ahead, leaving 3..=101 unseen.
        let newest = 2 + REPLAY_WINDOW;
        assert!(matches!(
            receive(&mut tracker, &advert(PEER_NONCE, LOCAL_NONCE, newest)?)?,
            Ok(Admission::Fresh(_))
        ));
        let fresh_at_newest = tracker.last_fresh_at();
        assert_ne!(fresh_at_newest, fresh_at);

        // Older than the window: seq 2 is exactly REPLAY_WINDOW behind.
        assert_rejected_without_side_effects(
            &mut tracker,
            &advert(PEER_NONCE, LOCAL_NONCE, 2)?,
            peer_addr()?,
            PacketRejection::TooOld,
        );

        // Unseen and inside the window: recorded, but not fresh.
        let reordered = advert(PEER_NONCE, LOCAL_NONCE, newest - 1)?;
        assert_eq!(receive(&mut tracker, &reordered)?, Ok(Admission::Reordered));
        assert_eq!(tracker.last_fresh_at(), fresh_at_newest);
        // The same reordered packet again is now a duplicate.
        assert_rejected_without_side_effects(
            &mut tracker,
            &reordered,
            peer_addr()?,
            PacketRejection::Duplicate,
        );
        // A reordered packet never moves the acknowledgement backwards.
        let (_, ours) = tracker.next_advert(advert_state())?;
        assert_eq!(
            HaPacket::decode(&ours, &PacketKey::new(TEST_KEY)?)?.ack_seq,
            newest
        );
        assert_eq!(tracker.rejections().duplicate, 2);
        assert_eq!(tracker.rejections().too_old, 1);
        Ok(())
    }

    #[test]
    fn previous_incarnation_replay_after_peer_restart_only_changes_counters() -> Result<()> {
        let mut tracker = tracker()?;
        fresh_session(&mut tracker)?;
        // Captured packets of the first incarnation (never delivered live).
        let captured: Vec<Vec<u8>> = (3..=6)
            .map(|seq| advert(PEER_NONCE, LOCAL_NONCE, seq))
            .collect::<Result<_>>()?;

        // The peer restarts with a new nonce and has not seen ours yet: the
        // packet is only an establishment candidate and the recorded session
        // is untouched (amended design spec 4.3).
        assert_eq!(
            receive(&mut tracker, &advert(RESTARTED_PEER_NONCE, ZERO_NONCE, 1)?)?,
            Ok(Admission::Unconfirmed { candidate: true })
        );
        assert_eq!(
            tracker.session.as_ref().map(|session| session.peer_nonce),
            Some(PEER_NONCE)
        );
        assert_eq!(tracker.retired, Vec::<Nonce>::new());

        // The restarted peer echoes our nonce: exactly this records the new
        // nonce, retires the old one and resets the window. It is fresh at
        // once, exactly as before the amendment.
        assert!(matches!(
            receive(&mut tracker, &advert(RESTARTED_PEER_NONCE, LOCAL_NONCE, 2)?)?,
            Ok(Admission::Fresh(_))
        ));
        assert_eq!(tracker.retired, vec![PEER_NONCE]);

        // Replaying the previous incarnation (higher seqs than the new
        // session, valid HMAC, our nonce echoed) changes only counters.
        for packet in &captured {
            assert_rejected_without_side_effects(
                &mut tracker,
                packet,
                peer_addr()?,
                PacketRejection::RetiredNonce,
            );
        }
        assert_eq!(tracker.rejections().retired_nonce, 4);

        // The live session keeps working: the replay never locked it out.
        assert!(matches!(
            receive(&mut tracker, &advert(RESTARTED_PEER_NONCE, LOCAL_NONCE, 3)?)?,
            Ok(Admission::Fresh(_))
        ));
        Ok(())
    }

    #[test]
    fn replayed_earlier_incarnation_never_locks_out_live_peer() -> Result<()> {
        let mut tracker = tracker()?;
        fresh_session(&mut tracker)?;

        // Captured packet of an earlier peer incarnation this node never saw
        // (its receiver_nonce predates our boot nonce, so it cannot be ours).
        // The attacker spoofs the configured source IP and replays it here.
        let captured = advert(EARLIER_INCARNATION_NONCE, ZERO_NONCE, 50)?;
        let before = session_view(&tracker);
        assert_eq!(
            tracker
                .receive(&captured, peer_addr()?, Instant::now())
                .map_err(|(cause, _)| cause),
            Ok(Admission::Unconfirmed { candidate: true })
        );
        // (a) the live recorded peer nonce is neither displaced nor retired,
        // and its replay window and liveness are untouched: the only state
        // change is the single-slot candidate and the reachable timestamp.
        assert_eq!(
            tracker.session.as_ref().map(|session| session.peer_nonce),
            Some(PEER_NONCE)
        );
        let after = session_view(&tracker);
        assert_eq!(after.0, before.0);
        assert_eq!(after.2, before.2);
        assert_eq!(
            after.1.map(|candidate| candidate.0),
            Some(EARLIER_INCARNATION_NONCE)
        );

        // (c) spammed copies are pure rejections: the counters reflect them
        // while no state changes at all.
        for _ in 0..2 {
            assert_rejected_without_side_effects(
                &mut tracker,
                &captured,
                peer_addr()?,
                PacketRejection::Duplicate,
            );
        }
        assert_eq!(tracker.rejections().duplicate, 2);

        // The recorded session still drives the echo, so the live peer keeps
        // seeing its own nonce.
        let (_, ours) = tracker.next_advert(advert_state())?;
        assert_eq!(
            HaPacket::decode(&ours, &PacketKey::new(TEST_KEY)?)?.receiver_nonce,
            PEER_NONCE
        );
        // (b) the live peer's next fresh advert is accepted: no lockout.
        assert!(matches!(
            receive(&mut tracker, &advert(PEER_NONCE, LOCAL_NONCE, 3)?)?,
            Ok(Admission::Fresh(_))
        ));

        // The same candidate-only packet is echoed when no session is
        // recorded, so a genuinely restarted peer still establishes a
        // session instead of being locked out.
        let mut restarted = PeerTracker::new(&runtime()?, "127.0.0.1".parse()?, [0x11; 16])?;
        assert_eq!(
            restarted
                .receive(&captured, peer_addr()?, Instant::now())
                .map_err(|(cause, _)| cause),
            Ok(Admission::Unconfirmed { candidate: true })
        );
        let (_, ours) = restarted.next_advert(advert_state())?;
        let ours = HaPacket::decode(&ours, &PacketKey::new(TEST_KEY)?)?;
        assert_eq!(ours.receiver_nonce, EARLIER_INCARNATION_NONCE);
        assert_eq!(ours.ack_seq, 0);
        // The restarted peer echoes our boot nonce: the session is recorded
        // and fresh immediately.
        assert!(matches!(
            receive(&mut restarted, &advert(EARLIER_INCARNATION_NONCE, [0x11; 16], 51)?)?,
            Ok(Admission::Fresh(ref packet)) if packet.seq == 51
        ));
        Ok(())
    }

    #[test]
    fn our_restart_makes_captured_packets_unfresh() -> Result<()> {
        // A packet captured while we ran with an older nonce echoes that old
        // nonce, so after our restart it can never become fresh: it is only
        // an establishment candidate.
        let mut restarted = PeerTracker::new(&runtime()?, "127.0.0.1".parse()?, [0x11; 16])?;
        assert_eq!(
            receive(&mut restarted, &advert(PEER_NONCE, LOCAL_NONCE, 9)?)?,
            Ok(Admission::Unconfirmed { candidate: true })
        );
        assert!(restarted.last_fresh_at().is_none());
        Ok(())
    }

    #[test]
    fn retired_nonce_overflow_fails_closed() -> Result<()> {
        let mut tracker = tracker()?;
        // First nonce plus RETIRED_NONCE_LIMIT restarts that echo our nonce
        // fill the retired set (the only recording path, amended spec 4.3).
        for incarnation in 0..=RETIRED_NONCE_LIMIT {
            let mut nonce = [0x40; 16];
            if let Some(byte) = nonce.get_mut(0) {
                *byte = u8::try_from(incarnation)?;
            }
            if let Some(byte) = nonce.get_mut(1) {
                *byte = 0x01;
            }
            assert!(matches!(
                receive(&mut tracker, &advert(nonce, LOCAL_NONCE, 1)?)?,
                Ok(Admission::Fresh(_))
            ));
        }
        assert_eq!(tracker.retired.len(), RETIRED_NONCE_LIMIT);

        // A further new nonce is rejected instead of evicting an entry --
        // and once the set is full not even a candidate is recorded.
        assert_rejected_without_side_effects(
            &mut tracker,
            &advert([0xee; 16], ZERO_NONCE, 1)?,
            peer_addr()?,
            PacketRejection::NonceLimit,
        );
        assert_rejected_without_side_effects(
            &mut tracker,
            &advert([0xee; 16], LOCAL_NONCE, 1)?,
            peer_addr()?,
            PacketRejection::NonceLimit,
        );
        assert_eq!(tracker.rejections().nonce_limit, 2);
        Ok(())
    }

    #[test]
    fn sequence_exhaustion_fails_closed() -> Result<()> {
        let mut tracker = tracker()?;
        let (first, _) = tracker.next_advert(advert_state())?;
        assert_eq!(first, 1);
        tracker.last_sent_seq = u64::MAX - 1;
        let (last, _) = tracker.next_advert(advert_state())?;
        assert_eq!(last, u64::MAX);
        assert!(tracker.next_advert(advert_state()).is_err());
        // It keeps failing: the sequence never wraps.
        assert!(tracker.next_advert(advert_state()).is_err());
        assert_eq!(tracker.last_sent_seq(), u64::MAX);
        Ok(())
    }

    #[test]
    fn boot_nonce_is_random_and_nonzero() -> Result<()> {
        let first = boot_nonce()?;
        let second = boot_nonce()?;
        assert_ne!(first, ZERO_NONCE);
        assert_ne!(first, second);
        assert!(PeerTracker::new(&runtime()?, "127.0.0.1".parse()?, ZERO_NONCE).is_err());
        Ok(())
    }
}
