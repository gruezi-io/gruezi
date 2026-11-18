//! Deterministic in-process datagram fabric for the failure matrix harness.
//!
//! The fabric replaces the UDP socket for two in-process nodes and carries
//! loss/duplicate/reorder knobs per direction. Semantics are deterministic:
//!
//! - `DropRule::All` discards at send time AND at delivery time. Every
//!   datagram queued before the direction started dropping carries an older
//!   drop epoch and is discarded when it would be delivered, so a mid-flight
//!   drop is exactly as deterministic as a send-time drop.
//! - `duplicate_next` delivers the next N datagrams of the direction twice.
//! - `hold_next` holds a datagram back until the next one is sent, which then
//!   jumps ahead of it (a deterministic reorder).
//!
//! No sockets, ports or subprocesses are involved, so the fabric runs safely
//! under `tokio::time::pause`.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

const FABRIC_PORT: u16 = 9375;

/// Synthetic endpoint address of node A (never bound to a socket).
#[must_use]
pub(crate) fn addr_a() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), FABRIC_PORT))
}

/// Synthetic endpoint address of node B (never bound to a socket).
#[must_use]
pub(crate) fn addr_b() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), FABRIC_PORT))
}

/// Which node sends through a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    A,
    B,
}

impl Side {
    fn direction(self, state: &FabricState) -> &Direction {
        match self {
            Self::A => &state.a_to_b,
            Self::B => &state.b_to_a,
        }
    }

    fn direction_mut(self, state: &mut FabricState) -> &mut Direction {
        match self {
            Self::A => &mut state.a_to_b,
            Self::B => &mut state.b_to_a,
        }
    }

    /// The direction datagrams travel to reach this node.
    pub(crate) const fn inbound(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

/// Loss knob for one direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DropRule {
    /// Deliver everything.
    #[default]
    None,
    /// Drop everything, at send time and at delivery time.
    All,
    /// Drop every `nth` datagram deterministically (`nth >= 2`).
    EveryNth(u64),
}

#[derive(Debug)]
struct QueuedDatagram {
    /// Drop epoch at enqueue time: datagrams older than the direction's
    /// current epoch were in flight when dropping started and are discarded.
    epoch: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Direction {
    rule: DropRule,
    drop_epoch: u64,
    sent: u64,
    delivered: u64,
    duplicate_next: usize,
    hold_next: usize,
    held: Option<QueuedDatagram>,
    tx: Option<mpsc::UnboundedSender<QueuedDatagram>>,
}

impl Direction {
    fn send(&mut self, bytes: Vec<u8>) {
        self.sent = self.sent.saturating_add(1);
        match self.rule {
            DropRule::All => return,
            DropRule::EveryNth(nth) if nth >= 2 && self.sent.is_multiple_of(nth) => return,
            DropRule::EveryNth(_) | DropRule::None => {}
        }
        if let Some(held) = self.held.take() {
            // Reorder knob: the newer datagram jumps ahead of the held one.
            let held_epoch = held.epoch;
            self.deliver(self.drop_epoch, bytes);
            self.deliver(held_epoch, held.bytes);
            return;
        }
        if self.hold_next > 0 {
            self.hold_next -= 1;
            self.held = Some(QueuedDatagram {
                epoch: self.drop_epoch,
                bytes,
            });
            return;
        }
        let duplicate = self.duplicate_next > 0;
        if duplicate {
            self.duplicate_next -= 1;
        }
        self.deliver(self.drop_epoch, bytes.clone());
        if duplicate {
            self.deliver(self.drop_epoch, bytes);
        }
    }

    fn deliver(&mut self, epoch: u64, bytes: Vec<u8>) {
        self.delivered = self.delivered.saturating_add(1);
        if let Some(tx) = &self.tx {
            // A dropped receiver is a crashed node: a send to it is a no-op,
            // exactly like a UDP datagram to a dead host.
            let _ = tx.send(QueuedDatagram { epoch, bytes });
        }
    }
}

#[derive(Debug, Default)]
struct FabricState {
    a_to_b: Direction,
    b_to_a: Direction,
}

/// Handle for reconfiguring the fabric while nodes run.
#[derive(Debug, Clone)]
pub(crate) struct InMemoryNetwork {
    state: Arc<Mutex<FabricState>>,
}

impl InMemoryNetwork {
    /// Build the fabric handle and its two endpoints. Node A owns the first
    /// endpoint.
    #[must_use]
    pub(crate) fn new_pair() -> (Self, InMemoryEndpoint, InMemoryEndpoint) {
        let (to_a, at_a) = mpsc::unbounded_channel();
        let (to_b, at_b) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(FabricState {
            a_to_b: Direction {
                tx: Some(to_b),
                ..Direction::default()
            },
            b_to_a: Direction {
                tx: Some(to_a),
                ..Direction::default()
            },
        }));
        let endpoint_a = InMemoryEndpoint {
            state: state.clone(),
            side: Side::A,
            rx: AsyncMutex::new(at_a),
            local_addr: addr_a(),
            peer_addr: addr_b(),
        };
        let endpoint_b = InMemoryEndpoint {
            state: state.clone(),
            side: Side::B,
            rx: AsyncMutex::new(at_b),
            local_addr: addr_b(),
            peer_addr: addr_a(),
        };
        (Self { state }, endpoint_a, endpoint_b)
    }

    /// Configure the loss knob for the direction `side` sends through.
    pub(crate) fn set_drop(&self, side: Side, rule: DropRule) {
        let mut state = self.lock_state();
        let direction = side.direction_mut(&mut state);
        direction.rule = rule;
        if matches!(rule, DropRule::All) {
            // Everything queued so far is in flight against a now-dropping
            // direction: retire it deterministically at delivery time.
            direction.drop_epoch = direction.drop_epoch.saturating_add(1);
        }
    }

    /// Deliver the next `count` datagrams from `side` twice.
    pub(crate) fn set_duplicate(&self, side: Side, count: usize) {
        let mut state = self.lock_state();
        side.direction_mut(&mut state).duplicate_next = count;
    }

    /// Hold the next `count` datagrams from `side` back one delivery each
    /// (deterministic reorder).
    pub(crate) fn set_reorder(&self, side: Side, count: usize) {
        let mut state = self.lock_state();
        side.direction_mut(&mut state).hold_next = count;
    }

    /// Datagrams delivered in the direction `side` sends through.
    pub(crate) fn delivered(&self, side: Side) -> u64 {
        let state = self.lock_state();
        side.direction(&state).delivered
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, FabricState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One node's view of the fabric: what it sends and what it receives.
#[derive(Debug)]
pub(crate) struct InMemoryEndpoint {
    state: Arc<Mutex<FabricState>>,
    side: Side,
    rx: AsyncMutex<mpsc::UnboundedReceiver<QueuedDatagram>>,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl InMemoryEndpoint {
    #[must_use]
    pub(crate) const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    #[must_use]
    pub(crate) const fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> usize {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.side.direction_mut(&mut state).send(bytes.to_vec());
        bytes.len()
    }

    pub(crate) async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        loop {
            let queued = {
                let mut rx = self.rx.lock().await;
                rx.recv().await
            };
            let Some(queued) = queued else {
                // The peer endpoint is gone: its node crashed or stopped.
                return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
            };
            // Delivery-time check: mid-flight datagrams from before the
            // direction started dropping are discarded deterministically.
            {
                let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                let direction = self.side.inbound().direction(&state);
                if queued.epoch < direction.drop_epoch || matches!(direction.rule, DropRule::All) {
                    continue;
                }
            }
            let len = queued.bytes.len();
            let slot = buffer.get_mut(..len).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "in-memory datagram exceeds the receive buffer",
                )
            })?;
            slot.copy_from_slice(&queued.bytes);
            return Ok((len, self.peer_addr));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DropRule, InMemoryEndpoint, InMemoryNetwork, Side};
    use anyhow::{Context, Result, anyhow};
    use tokio::time::Duration;

    /// The fabric's loss/duplicate/reorder knobs are deterministic, and a
    /// mid-flight datagram is dropped exactly like a send-time one.
    #[tokio::test(start_paused = true)]
    async fn transport_knobs_are_deterministic() -> Result<()> {
        let (net, a, b) = InMemoryNetwork::new_pair();

        // Loss knob: every second datagram is dropped.
        net.set_drop(Side::A, DropRule::EveryNth(2));
        a.send(b"keep-1");
        a.send(b"drop-2");
        a.send(b"keep-3");
        assert_eq!(recv_bytes(&b).await?, b"keep-1".to_vec());
        assert_eq!(recv_bytes(&b).await?, b"keep-3".to_vec());

        // Duplicate knob: the next datagram arrives twice.
        net.set_drop(Side::A, DropRule::None);
        net.set_duplicate(Side::A, 1);
        a.send(b"dup");
        assert_eq!(recv_bytes(&b).await?, b"dup".to_vec());
        assert_eq!(recv_bytes(&b).await?, b"dup".to_vec());

        // Reorder knob: the held datagram arrives behind the next one.
        net.set_reorder(Side::A, 1);
        a.send(b"first");
        a.send(b"second");
        assert_eq!(recv_bytes(&b).await?, b"second".to_vec());
        assert_eq!(recv_bytes(&b).await?, b"first".to_vec());

        // Mid-flight drop: a datagram queued before the direction started
        // dropping is discarded deterministically at delivery time.
        a.send(b"in-flight");
        net.set_drop(Side::A, DropRule::All);
        a.send(b"never");
        net.set_drop(Side::A, DropRule::None);
        a.send(b"after");
        assert_eq!(recv_bytes(&b).await?, b"after".to_vec());

        // Eight datagrams crossed the channel (the in-flight one was discarded
        // at delivery, "never" never entered it).
        assert_eq!(net.delivered(Side::A), 8);
        Ok(())
    }

    async fn recv_bytes(endpoint: &InMemoryEndpoint) -> Result<Vec<u8>> {
        let mut buffer = [0_u8; 1500];
        let (len, _) = tokio::time::timeout(Duration::from_secs(1), endpoint.recv(&mut buffer))
            .await
            .map_err(|_| anyhow!("in-memory receive timed out"))??;
        Ok(buffer
            .get(..len)
            .context("receive length exceeds buffer")?
            .to_vec())
    }
}
