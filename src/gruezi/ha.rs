use crate::config::{Config, HaAuthMode, Mode};
use anyhow::{Context, Result, anyhow, bail};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    cmp::Ordering,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

const PACKET_MAGIC: &[u8; 4] = b"GRHZ";
const MAX_ID_LEN: usize = 64;
const MAX_AUTH_TAG_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaState {
    Init,
    Backup,
    Master,
}

impl HaState {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Init => 0,
            Self::Backup => 1,
            Self::Master => 2,
        }
    }
}

impl TryFrom<u8> for HaState {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Init),
            1 => Ok(Self::Backup),
            2 => Ok(Self::Master),
            _ => bail!("invalid HA state value {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaAuth {
    None,
    SharedKey { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaRuntimeConfig {
    pub node_id: String,
    pub group_id: String,
    pub bind: String,
    pub peer: String,
    pub interface: String,
    pub addresses: Vec<String>,
    pub protocol_version: u8,
    pub priority: u8,
    pub preempt: bool,
    pub advert_interval_ms: u64,
    pub dead_factor: u8,
    pub hold_down_ms: u64,
    pub jitter_ms: u64,
    pub auth: HaAuth,
}

impl HaRuntimeConfig {
    #[must_use]
    pub fn advert_interval(&self) -> Duration {
        Duration::from_millis(self.advert_interval_ms)
    }

    #[must_use]
    pub fn dead_timeout(&self) -> Duration {
        self.advert_interval()
            .saturating_mul(u32::from(self.dead_factor))
    }

    #[must_use]
    pub fn hold_down(&self) -> Duration {
        Duration::from_millis(self.hold_down_ms)
    }

    #[must_use]
    pub fn jitter_for(&self, sequence: u64) -> Duration {
        if self.jitter_ms == 0 {
            return Duration::ZERO;
        }

        Duration::from_millis(sequence % (self.jitter_ms + 1))
    }

    #[must_use]
    pub fn next_advert_delay(&self, sequence: u64) -> Duration {
        self.advert_interval()
            .saturating_sub(self.jitter_for(sequence))
    }

    #[must_use]
    pub fn follower_deadline(&self, last_observed_at: Instant) -> Instant {
        last_observed_at + self.dead_timeout() + self.hold_down()
    }
}

impl TryFrom<&Config> for HaRuntimeConfig {
    type Error = anyhow::Error;

    fn try_from(config: &Config) -> Result<Self> {
        if config.mode != Mode::Ha {
            bail!("HA runtime config requires mode 'ha'");
        }

        let node_id = config
            .node
            .id
            .as_deref()
            .map(str::trim)
            .filter(|node_id| !node_id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("mode 'ha' requires node.id"))?;

        let peer = config
            .ha
            .peer
            .as_deref()
            .map(str::trim)
            .filter(|peer| !peer.is_empty())
            .ok_or_else(|| anyhow::anyhow!("mode 'ha' requires ha.peer"))?;

        let auth = match config.ha.auth.mode {
            HaAuthMode::None => HaAuth::None,
            HaAuthMode::SharedKey => {
                let key = config
                    .ha
                    .auth
                    .key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "mode 'ha' requires ha.auth.key when ha.auth.mode is 'shared_key'"
                        )
                    })?;

                HaAuth::SharedKey {
                    key: key.to_owned(),
                }
            }
        };

        Ok(Self {
            node_id: node_id.to_owned(),
            group_id: config.ha.group_id.clone(),
            bind: config.ha.bind.clone(),
            peer: peer.to_owned(),
            interface: config.ha.interface.clone(),
            addresses: config.ha.addresses.clone(),
            protocol_version: config.ha.protocol_version,
            priority: config.ha.priority,
            preempt: config.ha.preempt,
            advert_interval_ms: config.ha.advert_interval_ms,
            dead_factor: config.ha.dead_factor,
            hold_down_ms: config.ha.hold_down_ms,
            jitter_ms: config.ha.jitter_ms,
            auth,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaPacket {
    pub protocol_version: u8,
    pub state: HaState,
    pub priority: u8,
    pub dead_factor: u8,
    pub advert_interval_ms: u32,
    pub sequence: u64,
    pub node_id: String,
    pub group_id: String,
    pub auth_tag: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerObservation {
    node_id: String,
    state: HaState,
    priority: u8,
    observed_at: Instant,
}

impl HaPacket {
    /// Encode an HA packet into the wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if any field exceeds the supported limits.
    pub fn encode(&self) -> Result<Vec<u8>> {
        validate_packet_field(&self.node_id, "node_id", MAX_ID_LEN)?;
        validate_packet_field(&self.group_id, "group_id", MAX_ID_LEN)?;

        if self.auth_tag.len() > MAX_AUTH_TAG_LEN {
            bail!("auth_tag exceeds {MAX_AUTH_TAG_LEN} bytes");
        }

        let node_id_len = u8::try_from(self.node_id.len())
            .map_err(|_| anyhow::anyhow!("node_id exceeds {} bytes", u8::MAX))?;
        let group_id_len = u8::try_from(self.group_id.len())
            .map_err(|_| anyhow::anyhow!("group_id exceeds {} bytes", u8::MAX))?;
        let auth_tag_len = u16::try_from(self.auth_tag.len())
            .map_err(|_| anyhow::anyhow!("auth_tag exceeds {} bytes", u16::MAX))?;

        let mut bytes = Vec::with_capacity(
            PACKET_MAGIC.len()
                + 1
                + 1
                + 1
                + 1
                + 4
                + 8
                + 1
                + usize::from(node_id_len)
                + 1
                + usize::from(group_id_len)
                + 2
                + usize::from(auth_tag_len),
        );

        bytes.extend_from_slice(PACKET_MAGIC);
        bytes.push(self.protocol_version);
        bytes.push(self.state.as_u8());
        bytes.push(self.priority);
        bytes.push(self.dead_factor);
        bytes.extend_from_slice(&self.advert_interval_ms.to_be_bytes());
        bytes.extend_from_slice(&self.sequence.to_be_bytes());
        bytes.push(node_id_len);
        bytes.extend_from_slice(self.node_id.as_bytes());
        bytes.push(group_id_len);
        bytes.extend_from_slice(self.group_id.as_bytes());
        bytes.extend_from_slice(&auth_tag_len.to_be_bytes());
        bytes.extend_from_slice(&self.auth_tag);

        Ok(bytes)
    }

    /// Decode an HA packet from the wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if the packet is malformed or incomplete.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = PacketCursor::new(bytes);

        if cursor.read_exact(PACKET_MAGIC.len())? != PACKET_MAGIC {
            bail!("invalid packet magic");
        }

        let protocol_version = cursor.read_u8()?;
        let state = HaState::try_from(cursor.read_u8()?)?;
        let priority = cursor.read_u8()?;
        let dead_factor = cursor.read_u8()?;
        let advert_interval_ms = cursor.read_u32()?;
        let sequence = cursor.read_u64()?;
        let node_id_len = usize::from(cursor.read_u8()?);
        let node_id = cursor.read_string(node_id_len)?;
        let group_id_len = usize::from(cursor.read_u8()?);
        let group_id = cursor.read_string(group_id_len)?;
        let auth_tag_len = usize::from(cursor.read_u16()?);
        let auth_tag = cursor.read_vec(auth_tag_len)?;

        if !cursor.is_empty() {
            bail!("packet has trailing bytes");
        }

        validate_packet_field(&node_id, "node_id", MAX_ID_LEN)?;
        validate_packet_field(&group_id, "group_id", MAX_ID_LEN)?;

        if auth_tag.len() > MAX_AUTH_TAG_LEN {
            bail!("auth_tag exceeds {MAX_AUTH_TAG_LEN} bytes");
        }

        Ok(Self {
            protocol_version,
            state,
            priority,
            dead_factor,
            advert_interval_ms,
            sequence,
            node_id,
            group_id,
            auth_tag,
        })
    }
}

fn validate_packet_field(value: &str, field_name: &str, max_len: usize) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field_name} cannot be empty");
    }

    if value.len() > max_len {
        bail!("{field_name} exceeds {max_len} bytes");
    }

    Ok(())
}

/// Run the HA control loop.
///
/// This keeps the process alive, exchanges UDP advertisements with the peer,
/// tracks peer liveness, and drives the local HA state machine.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound or packet I/O fails.
pub async fn run(runtime: HaRuntimeConfig) -> Result<()> {
    let (socket, local_bind) = bind_ha_socket(&runtime.bind)?;
    let peer_addr = runtime
        .peer
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid HA peer address {}", runtime.peer))?;
    let startup_at = Instant::now();
    let mut buffer = [0_u8; 1_024];
    let mut sequence = 0_u64;
    let mut state = HaState::Init;
    let mut peer_observation: Option<PeerObservation> = None;

    info!(
        node_id = %runtime.node_id,
        bind = %local_bind,
        peer = %peer_addr,
        group_id = %runtime.group_id,
        "HA runtime loop started"
    );

    loop {
        let desired_state = desired_state(
            &runtime,
            peer_observation.as_ref(),
            startup_at,
            Instant::now(),
        );
        log_state_change(state, desired_state, &runtime.node_id);
        state = desired_state;

        let sleep = tokio::time::sleep(runtime.next_advert_delay(sequence + 1));
        tokio::pin!(sleep);

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(node_id = %runtime.node_id, "HA runtime loop received shutdown signal");
                break;
            }
            recv = socket.recv_from(&mut buffer) => {
                let (len, from) = recv.context("failed to receive HA packet")?;
                let now = Instant::now();
                let payload = buffer
                    .get(..len)
                    .ok_or_else(|| anyhow!("received invalid HA payload length"))?;

                if let Err(error) = handle_incoming_packet(
                    &runtime,
                    payload,
                    from,
                    now,
                    &mut peer_observation,
                ) {
                    warn!(%error, from = %from, "ignoring invalid HA packet");
                }
            }
            () = &mut sleep => {
                sequence = sequence.saturating_add(1);
                let packet = build_packet(&runtime, state, sequence)?;
                let encoded = packet.encode()?;
                socket
                    .send_to(&encoded, peer_addr)
                    .await
                    .with_context(|| format!("failed to send HA packet to {peer_addr}"))?;

                debug!(
                    node_id = %runtime.node_id,
                    state = ?state,
                    sequence,
                    peer = %peer_addr,
                    "sent HA advertisement"
                );
            }
        }
    }

    Ok(())
}

fn handle_incoming_packet(
    runtime: &HaRuntimeConfig,
    payload: &[u8],
    from: std::net::SocketAddr,
    observed_at: Instant,
    peer_observation: &mut Option<PeerObservation>,
) -> Result<()> {
    let packet = HaPacket::decode(payload)?;

    if packet.protocol_version != runtime.protocol_version {
        bail!(
            "unexpected HA protocol version {} from {}",
            packet.protocol_version,
            from
        );
    }

    if packet.group_id != runtime.group_id {
        bail!("unexpected HA group {}", packet.group_id);
    }

    if packet.node_id == runtime.node_id {
        bail!("received looped HA packet from local node");
    }

    if !verify_auth_tag(runtime, &packet) {
        bail!("HA packet authentication failed");
    }

    *peer_observation = Some(PeerObservation {
        node_id: packet.node_id,
        state: packet.state,
        priority: packet.priority,
        observed_at,
    });

    debug!(
        node_id = %runtime.node_id,
        peer = %from,
        peer_state = ?packet.state,
        peer_priority = packet.priority,
        "received HA advertisement"
    );

    Ok(())
}

fn build_packet(runtime: &HaRuntimeConfig, state: HaState, sequence: u64) -> Result<HaPacket> {
    let advert_interval_ms = u32::try_from(runtime.advert_interval_ms)
        .map_err(|_| anyhow!("HA advert interval exceeds u32"))?;
    let auth_tag = auth_tag(runtime, state, sequence, advert_interval_ms);

    Ok(HaPacket {
        protocol_version: runtime.protocol_version,
        state,
        priority: runtime.priority,
        dead_factor: runtime.dead_factor,
        advert_interval_ms,
        sequence,
        node_id: runtime.node_id.clone(),
        group_id: runtime.group_id.clone(),
        auth_tag,
    })
}

fn desired_state(
    runtime: &HaRuntimeConfig,
    peer_observation: Option<&PeerObservation>,
    startup_at: Instant,
    now: Instant,
) -> HaState {
    let startup_deadline = runtime.follower_deadline(startup_at);

    let Some(peer) =
        peer_observation.filter(|peer| runtime.follower_deadline(peer.observed_at) > now)
    else {
        return if now >= startup_deadline {
            HaState::Master
        } else {
            HaState::Backup
        };
    };

    match peer.state {
        HaState::Master => {
            if runtime.preempt
                && outranks(
                    runtime.priority,
                    &runtime.node_id,
                    peer.priority,
                    &peer.node_id,
                )
            {
                HaState::Master
            } else {
                HaState::Backup
            }
        }
        HaState::Init | HaState::Backup => {
            if outranks(
                runtime.priority,
                &runtime.node_id,
                peer.priority,
                &peer.node_id,
            ) {
                HaState::Master
            } else {
                HaState::Backup
            }
        }
    }
}

fn outranks(
    local_priority: u8,
    local_node_id: &str,
    peer_priority: u8,
    peer_node_id: &str,
) -> bool {
    match local_priority.cmp(&peer_priority) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => local_node_id > peer_node_id,
    }
}

fn auth_tag(
    runtime: &HaRuntimeConfig,
    state: HaState,
    sequence: u64,
    advert_interval_ms: u32,
) -> Vec<u8> {
    match &runtime.auth {
        HaAuth::None => Vec::new(),
        HaAuth::SharedKey { key } => {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            runtime.group_id.hash(&mut hasher);
            runtime.node_id.hash(&mut hasher);
            runtime.priority.hash(&mut hasher);
            runtime.dead_factor.hash(&mut hasher);
            advert_interval_ms.hash(&mut hasher);
            state.as_u8().hash(&mut hasher);
            sequence.hash(&mut hasher);
            hasher.finish().to_be_bytes().to_vec()
        }
    }
}

fn verify_auth_tag(runtime: &HaRuntimeConfig, packet: &HaPacket) -> bool {
    auth_tag(
        runtime,
        packet.state,
        packet.sequence,
        packet.advert_interval_ms,
    ) == packet.auth_tag
}

fn log_state_change(previous: HaState, current: HaState, node_id: &str) {
    if previous != current {
        info!(node_id, ?previous, ?current, "HA state changed");
    }
}

fn bind_ha_socket(bind: &str) -> Result<(UdpSocket, String)> {
    let bind_addr = bind
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid HA bind address {bind}"))?;

    if bind_addr.ip().is_unspecified()
        && bind_addr.is_ipv4()
        && let Ok(socket) = bind_udp_socket(
            Domain::IPV6,
            std::net::SocketAddr::from(([0_u16; 8], bind_addr.port())),
            Some(false),
        )
    {
        let display = format_socket_addr(socket.local_addr()?);
        return Ok((socket, display));
    }

    let domain = if bind_addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let only_v6 = bind_addr.is_ipv6().then_some(true);
    let socket = bind_udp_socket(domain, bind_addr, only_v6)?;
    let display = format_socket_addr(socket.local_addr()?);

    Ok((socket, display))
}

fn bind_udp_socket(
    domain: Domain,
    socket_addr: std::net::SocketAddr,
    only_v6: Option<bool>,
) -> Result<UdpSocket> {
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    if let Some(only_v6) = only_v6 {
        socket.set_only_v6(only_v6)?;
    }

    socket.bind(&SockAddr::from(socket_addr))?;
    socket.set_nonblocking(true)?;

    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket).map_err(Into::into)
}

fn format_socket_addr(addr: std::net::SocketAddr) -> String {
    if addr.is_ipv6() {
        format!("[{}]:{}", addr.ip(), addr.port())
    } else {
        format!("{}:{}", addr.ip(), addr.port())
    }
}

#[derive(Debug)]
struct PacketCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PacketCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.offset.saturating_add(len);
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| anyhow::anyhow!("packet is truncated"))?;
        self.offset = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8> {
        self.read_exact(1)?
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("packet is truncated"))
    }

    fn read_u16(&mut self) -> Result<u16> {
        let bytes: [u8; 2] = self
            .read_exact(2)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("packet is truncated"))?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes: [u8; 4] = self
            .read_exact(4)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("packet is truncated"))?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes: [u8; 8] = self
            .read_exact(8)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("packet is truncated"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn read_string(&mut self, len: usize) -> Result<String> {
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec()).map_err(Into::into)
    }

    fn read_vec(&mut self, len: usize) -> Result<Vec<u8>> {
        Ok(self.read_exact(len)?.to_vec())
    }

    const fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{HaAuth, HaPacket, HaRuntimeConfig, HaState, PeerObservation, desired_state};
    use crate::config::Config;
    use anyhow::Result;
    use std::time::{Duration, Instant};

    #[test]
    fn builds_runtime_config_from_ha_yaml() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  bind: 0.0.0.0:9375
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
    mode: shared_key
    key: super-secret
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;

        assert_eq!(runtime.node_id, "node-a");
        assert_eq!(runtime.group_id, "cluster-ha");
        assert_eq!(runtime.peer, "10.0.0.2:9375");
        assert!(matches!(runtime.auth, HaAuth::SharedKey { .. }));

        Ok(())
    }

    #[test]
    fn encodes_and_decodes_packet() -> Result<()> {
        let packet = HaPacket {
            protocol_version: 1,
            state: HaState::Backup,
            priority: 110,
            dead_factor: 3,
            advert_interval_ms: 1_000,
            sequence: 42,
            node_id: "node-a".to_owned(),
            group_id: "cluster-ha".to_owned(),
            auth_tag: vec![1, 2, 3, 4],
        };

        let encoded = packet.encode()?;
        let decoded = HaPacket::decode(&encoded)?;

        assert_eq!(decoded, packet);

        Ok(())
    }

    #[test]
    fn rejects_packet_with_invalid_magic() {
        let result = HaPacket::decode(b"nope");
        assert!(result.is_err());
    }

    #[test]
    fn calculates_ha_timers() -> Result<()> {
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
        let observed_at = Instant::now();

        assert_eq!(runtime.advert_interval(), Duration::from_secs(1));
        assert_eq!(runtime.dead_timeout(), Duration::from_secs(3));
        assert_eq!(runtime.hold_down(), Duration::from_secs(3));
        assert_eq!(runtime.next_advert_delay(50), Duration::from_millis(950));
        assert!(runtime.follower_deadline(observed_at) >= observed_at + Duration::from_secs(6));

        Ok(())
    }

    #[test]
    fn higher_priority_backup_promotes_to_master() -> Result<()> {
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
            state: HaState::Backup,
            priority: 100,
            observed_at: Instant::now(),
        };

        assert_eq!(
            desired_state(&runtime, Some(&peer), Instant::now(), Instant::now()),
            HaState::Master
        );

        Ok(())
    }

    #[test]
    fn lower_priority_node_stays_backup_when_peer_is_master() -> Result<()> {
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
            state: HaState::Master,
            priority: 110,
            observed_at: Instant::now(),
        };

        assert_eq!(
            desired_state(&runtime, Some(&peer), Instant::now(), Instant::now()),
            HaState::Backup
        );

        Ok(())
    }
}
