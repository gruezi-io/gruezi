//! HA peer wire protocol, version 1.
//!
//! The protocol is a prototype: its wire format is unstable and carries no
//! compatibility promise. Version 1 is rewritten in place; there is no
//! negotiation and no second layout.
//!
//! Every datagram is a single ADVERT (kind 1). Handoff state travels inside the
//! periodic advert so that a lost datagram is repaired by the next one. Layout
//! (big-endian, in order):
//!
//! ```text
//! magic "GRHZ" | version u8 | kind u8 | flags u8 (0) | reserved u8 (0) | total_len u16
//! group_id (u8 len, 1..=64 UTF-8) | sender_id (u8 len) | receiver_id (u8 len)
//! sender_nonce [16] | receiver_nonce [16] | seq u64 | ack_seq u64
//! role u8 | authority_source u8 | resource_state u8 | priority u8 (1..=255)
//! term u64 | transfer_id u64 | transfer_phase u8 | tag [32]
//! ```
//!
//! `tag = HMAC-SHA-256(key, b"gruezi-ha-v1\0" || every byte before the tag)`, so
//! the version, kind, length and every field are authenticated. Framing is fully
//! validated before the tag is checked, and the tag is compared in constant time
//! through `Mac::verify_slice`.

use super::election::HaState;
use crate::gruezi::ownership::{AuthoritySource, AuthorityState, ResourceState};
use anyhow::{Result, anyhow, bail};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const WIRE_VERSION: u8 = 1;
/// The only datagram kind. Handoff phases are state inside the advert.
pub const KIND_ADVERT: u8 = 1;
/// Upper bound for an encoded datagram; anything longer is rejected unread.
pub const MAX_DATAGRAM_LEN: usize = 1_200;
/// Receive buffer size. Larger than [`MAX_DATAGRAM_LEN`] so an oversize
/// datagram is observed as oversize instead of being silently truncated into
/// something that might parse.
pub const RECEIVE_BUFFER_LEN: usize = 1_500;
pub const NONCE_LEN: usize = 16;
pub const TAG_LEN: usize = 32;

const PACKET_MAGIC: &[u8; 4] = b"GRHZ";
const MAX_ID_LEN: usize = 64;
/// Domain separation: a tag computed for this protocol version can never be
/// confused with an HMAC the same key produces for another purpose.
const MAC_CONTEXT: &[u8] = b"gruezi-ha-v1\0";

/// Per-process random session identifier (see the session module).
pub type Nonce = [u8; NONCE_LEN];
/// `receiver_nonce` value meaning "the sender has not seen our nonce yet".
pub const ZERO_NONCE: Nonce = [0; NONCE_LEN];

/// Why a datagram was rejected. Every cause maps to exactly one counter and a
/// rejection never changes any other state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketRejection {
    /// Source IP is not the configured peer IP (step 1).
    WrongSource,
    /// Truncation, trailing bytes, oversize, length mismatch, bad magic,
    /// invalid id length/UTF-8, zero sender nonce or zero seq (step 2).
    Malformed,
    /// Unknown version, kind, flags, reserved byte or enum value (step 2).
    Unsupported,
    /// HMAC verification failed (step 3).
    BadTag,
    /// `group_id` differs from ours (step 4).
    WrongGroup,
    /// `sender_id` equals our own `node.id` (step 4).
    DuplicateNodeId,
    /// `sender_id` differs from the configured `peer_id` (step 4).
    WrongSender,
    /// `receiver_id` differs from our `node.id` (step 4).
    WrongReceiver,
    /// Sender nonce belongs to a retired peer session (step 5).
    RetiredNonce,
    /// A new sender nonce arrived while the retired set is full (step 5).
    NonceLimit,
    /// Sequence number already accepted in this session (step 5).
    Duplicate,
    /// Sequence number older than the replay window (step 5).
    TooOld,
}

impl PacketRejection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WrongSource => "wrong_source",
            Self::Malformed => "malformed",
            Self::Unsupported => "unsupported",
            Self::BadTag => "bad_tag",
            Self::WrongGroup => "wrong_group",
            Self::DuplicateNodeId => "duplicate_node_id",
            Self::WrongSender => "wrong_sender",
            Self::WrongReceiver => "wrong_receiver",
            Self::RetiredNonce => "retired_nonce",
            Self::NonceLimit => "nonce_limit",
            Self::Duplicate => "duplicate",
            Self::TooOld => "too_old",
        }
    }
}

impl std::fmt::Display for PacketRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for PacketRejection {}

/// Wire `authority_source`: 0 none, 1 peer, 2 assumed, 3 external.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertAuthoritySource {
    None,
    Peer,
    Assumed,
    External,
}

impl AdvertAuthoritySource {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Peer => 1,
            Self::Assumed => 2,
            Self::External => 3,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Peer),
            2 => Some(Self::Assumed),
            3 => Some(Self::External),
            _ => None,
        }
    }

    /// Advertise a source only while authority is valid; pending or uncertain
    /// authority must never look like a grant to the peer.
    #[must_use]
    pub const fn from_authority(authority: &AuthorityState) -> Self {
        match authority {
            AuthorityState::Valid { source, .. } => match source {
                AuthoritySource::Peer => Self::Peer,
                AuthoritySource::Assumed => Self::Assumed,
                AuthoritySource::External => Self::External,
            },
            AuthorityState::None | AuthorityState::Pending | AuthorityState::Uncertain { .. } => {
                Self::None
            }
        }
    }
}

/// Wire `resource_state`: 0 unmanaged, 1 absent, 2 acquiring, 3 present,
/// 4 releasing, 5 uncertain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertResourceState {
    Unmanaged,
    Absent,
    Acquiring,
    Present,
    Releasing,
    Uncertain,
}

impl AdvertResourceState {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Unmanaged => 0,
            Self::Absent => 1,
            Self::Acquiring => 2,
            Self::Present => 3,
            Self::Releasing => 4,
            Self::Uncertain => 5,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Unmanaged),
            1 => Some(Self::Absent),
            2 => Some(Self::Acquiring),
            3 => Some(Self::Present),
            4 => Some(Self::Releasing),
            5 => Some(Self::Uncertain),
            _ => None,
        }
    }
}

impl From<&ResourceState> for AdvertResourceState {
    fn from(state: &ResourceState) -> Self {
        match state {
            ResourceState::Unmanaged => Self::Unmanaged,
            ResourceState::Absent => Self::Absent,
            ResourceState::Acquiring { .. } => Self::Acquiring,
            ResourceState::Present { .. } => Self::Present,
            ResourceState::Releasing { .. } => Self::Releasing,
            ResourceState::Uncertain { .. } => Self::Uncertain,
        }
    }
}

/// Wire `transfer_phase`: 0 none, 1 requesting, 2 released, 3 owned, 4 returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    None,
    Requesting,
    Released,
    Owned,
    Returned,
}

impl TransferPhase {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Requesting => 1,
            Self::Released => 2,
            Self::Owned => 3,
            Self::Returned => 4,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Requesting),
            2 => Some(Self::Released),
            3 => Some(Self::Owned),
            4 => Some(Self::Returned),
            _ => None,
        }
    }
}

/// Pre-keyed HMAC-SHA-256 state. Cloned per datagram so the key schedule runs
/// once per process. `Debug` never prints key material.
#[derive(Clone)]
pub struct PacketKey {
    mac: Hmac<Sha256>,
}

impl std::fmt::Debug for PacketKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PacketKey(<redacted>)")
    }
}

impl PacketKey {
    /// Build the packet key from the raw `ha.auth.key` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is empty.
    pub fn new(key: &[u8]) -> Result<Self> {
        if key.is_empty() {
            bail!("HA packet key cannot be empty");
        }
        let mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key)
            .map_err(|_| anyhow!("HA packet key has an unsupported length"))?;
        Ok(Self { mac })
    }

    fn mac_over(&self, parts: &[&[u8]]) -> Hmac<Sha256> {
        let mut mac = self.mac.clone();
        for part in parts {
            mac.update(part);
        }
        mac
    }

    fn compute(&self, parts: &[&[u8]]) -> [u8; TAG_LEN] {
        self.mac_over(parts).finalize().into_bytes().into()
    }

    /// Constant-time comparison through `Mac::verify_slice`.
    fn verify(&self, parts: &[&[u8]], tag: &[u8]) -> bool {
        self.mac_over(parts).verify_slice(tag).is_ok()
    }

    fn packet_tag(&self, signed: &[u8]) -> [u8; TAG_LEN] {
        self.compute(&[MAC_CONTEXT, signed])
    }

    fn verify_packet(&self, signed: &[u8], tag: &[u8]) -> bool {
        self.verify(&[MAC_CONTEXT, signed], tag)
    }
}

/// One decoded ADVERT (the tag is not part of the value: a decoded packet has
/// always been authenticated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaPacket {
    pub group_id: String,
    pub sender_id: String,
    pub receiver_id: String,
    pub sender_nonce: Nonce,
    pub receiver_nonce: Nonce,
    pub seq: u64,
    pub ack_seq: u64,
    pub role: HaState,
    pub authority_source: AdvertAuthoritySource,
    pub resource_state: AdvertResourceState,
    pub priority: u8,
    pub term: u64,
    pub transfer_id: u64,
    pub transfer_phase: TransferPhase,
}

/// magic + version + kind + flags + reserved + `total_len`.
const HEADER_LEN: usize = PACKET_MAGIC.len() + 1 + 1 + 1 + 1 + 2;
/// Everything except the three id strings.
const FIXED_LEN: usize =
    HEADER_LEN + 3 + NONCE_LEN * 2 + 8 + 8 + 1 + 1 + 1 + 1 + 8 + 8 + 1 + TAG_LEN;

impl HaPacket {
    /// Encode and authenticate the advert.
    ///
    /// # Errors
    ///
    /// Returns an error if an id is empty or longer than 64 bytes, `seq` or
    /// `priority` is zero, the sender nonce is zero, or the datagram would
    /// exceed [`MAX_DATAGRAM_LEN`].
    pub fn encode(&self, key: &PacketKey) -> Result<Vec<u8>> {
        let group_id_len = encoded_id_len(&self.group_id, "group_id")?;
        let sender_id_len = encoded_id_len(&self.sender_id, "sender_id")?;
        let receiver_id_len = encoded_id_len(&self.receiver_id, "receiver_id")?;
        if self.seq == 0 {
            bail!("HA advert seq starts at 1");
        }
        if self.priority == 0 {
            bail!("HA advert priority must be 1..=255");
        }
        if self.sender_nonce == ZERO_NONCE {
            bail!("HA advert sender nonce cannot be zero");
        }

        let total_len = FIXED_LEN
            + usize::from(group_id_len)
            + usize::from(sender_id_len)
            + usize::from(receiver_id_len);
        if total_len > MAX_DATAGRAM_LEN {
            bail!("HA advert exceeds {MAX_DATAGRAM_LEN} bytes");
        }
        let total_len_field =
            u16::try_from(total_len).map_err(|_| anyhow!("HA advert length exceeds u16"))?;

        let mut bytes = Vec::with_capacity(total_len);
        bytes.extend_from_slice(PACKET_MAGIC);
        bytes.push(WIRE_VERSION);
        bytes.push(KIND_ADVERT);
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&total_len_field.to_be_bytes());
        bytes.push(group_id_len);
        bytes.extend_from_slice(self.group_id.as_bytes());
        bytes.push(sender_id_len);
        bytes.extend_from_slice(self.sender_id.as_bytes());
        bytes.push(receiver_id_len);
        bytes.extend_from_slice(self.receiver_id.as_bytes());
        bytes.extend_from_slice(&self.sender_nonce);
        bytes.extend_from_slice(&self.receiver_nonce);
        bytes.extend_from_slice(&self.seq.to_be_bytes());
        bytes.extend_from_slice(&self.ack_seq.to_be_bytes());
        bytes.push(self.role.as_u8());
        bytes.push(self.authority_source.as_u8());
        bytes.push(self.resource_state.as_u8());
        bytes.push(self.priority);
        bytes.extend_from_slice(&self.term.to_be_bytes());
        bytes.extend_from_slice(&self.transfer_id.to_be_bytes());
        bytes.push(self.transfer_phase.as_u8());
        let tag = key.packet_tag(&bytes);
        bytes.extend_from_slice(&tag);

        Ok(bytes)
    }

    /// Validate framing, then authenticate, then return the advert.
    ///
    /// Framing (length bounds, version, kind, flags, reserved, enum values) is
    /// checked completely before the tag, and nothing is returned unless the
    /// tag verifies.
    ///
    /// # Errors
    ///
    /// Returns [`PacketRejection::Malformed`] or
    /// [`PacketRejection::Unsupported`] for framing failures and
    /// [`PacketRejection::BadTag`] when authentication fails.
    pub fn decode(bytes: &[u8], key: &PacketKey) -> Result<Self, PacketRejection> {
        let (packet, signed, tag) = parse_frame(bytes)?;
        if !key.verify_packet(signed, tag) {
            return Err(PacketRejection::BadTag);
        }
        Ok(packet)
    }
}

fn encoded_id_len(value: &str, field_name: &str) -> Result<u8> {
    if value.is_empty() || value.len() > MAX_ID_LEN {
        bail!("{field_name} must contain 1..={MAX_ID_LEN} bytes");
    }
    u8::try_from(value.len()).map_err(|_| anyhow!("{field_name} exceeds {} bytes", u8::MAX))
}

/// Step 2 of the receive pipeline: bounded framing and value validation.
/// Returns the unauthenticated packet, the signed bytes and the tag.
fn parse_frame(bytes: &[u8]) -> Result<(HaPacket, &[u8], &[u8]), PacketRejection> {
    if bytes.len() > MAX_DATAGRAM_LEN {
        return Err(PacketRejection::Malformed);
    }
    let signed_len = bytes
        .len()
        .checked_sub(TAG_LEN)
        .ok_or(PacketRejection::Malformed)?;
    let (signed, tag) = bytes
        .split_at_checked(signed_len)
        .ok_or(PacketRejection::Malformed)?;

    let mut cursor = PacketCursor::new(signed);
    if cursor.read_exact(PACKET_MAGIC.len())? != PACKET_MAGIC {
        return Err(PacketRejection::Malformed);
    }
    if cursor.read_u8()? != WIRE_VERSION
        || cursor.read_u8()? != KIND_ADVERT
        || cursor.read_u8()? != 0
        || cursor.read_u8()? != 0
    {
        return Err(PacketRejection::Unsupported);
    }
    if usize::from(cursor.read_u16()?) != bytes.len() {
        return Err(PacketRejection::Malformed);
    }

    let group_id = cursor.read_id()?;
    let sender_id = cursor.read_id()?;
    let receiver_id = cursor.read_id()?;
    let sender_nonce = cursor.read_array::<NONCE_LEN>()?;
    let receiver_nonce = cursor.read_array::<NONCE_LEN>()?;
    let seq = cursor.read_u64()?;
    let ack_seq = cursor.read_u64()?;
    if sender_nonce == ZERO_NONCE || seq == 0 {
        return Err(PacketRejection::Malformed);
    }
    let role = HaState::try_from(cursor.read_u8()?).map_err(|_| PacketRejection::Unsupported)?;
    let authority_source =
        AdvertAuthoritySource::from_u8(cursor.read_u8()?).ok_or(PacketRejection::Unsupported)?;
    let resource_state =
        AdvertResourceState::from_u8(cursor.read_u8()?).ok_or(PacketRejection::Unsupported)?;
    let priority = cursor.read_u8()?;
    if priority == 0 {
        return Err(PacketRejection::Unsupported);
    }
    let term = cursor.read_u64()?;
    let transfer_id = cursor.read_u64()?;
    let transfer_phase =
        TransferPhase::from_u8(cursor.read_u8()?).ok_or(PacketRejection::Unsupported)?;
    if !cursor.is_empty() {
        return Err(PacketRejection::Malformed);
    }

    Ok((
        HaPacket {
            group_id,
            sender_id,
            receiver_id,
            sender_nonce,
            receiver_nonce,
            seq,
            ack_seq,
            role,
            authority_source,
            resource_state,
            priority,
            term,
            transfer_id,
            transfer_phase,
        },
        signed,
        tag,
    ))
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

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], PacketRejection> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(PacketRejection::Malformed)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(PacketRejection::Malformed)?;
        self.offset = end;
        Ok(slice)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], PacketRejection> {
        self.read_exact(N)?
            .try_into()
            .map_err(|_| PacketRejection::Malformed)
    }

    fn read_u8(&mut self) -> Result<u8, PacketRejection> {
        let [value] = self.read_array::<1>()?;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, PacketRejection> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, PacketRejection> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    fn read_id(&mut self) -> Result<String, PacketRejection> {
        let len = usize::from(self.read_u8()?);
        if len == 0 || len > MAX_ID_LEN {
            return Err(PacketRejection::Malformed);
        }
        let bytes = self.read_exact(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| PacketRejection::Malformed)
    }

    const fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::{
        AdvertAuthoritySource, AdvertResourceState, HEADER_LEN, HaPacket, MAX_DATAGRAM_LEN,
        PacketKey, PacketRejection, TAG_LEN, TransferPhase, ZERO_NONCE,
    };
    use crate::gruezi::ha::HaState;
    use anyhow::{Context, Result, anyhow};

    pub(in crate::gruezi::ha) const TEST_KEY: &[u8] = b"shared-secret-for-tests";

    pub(in crate::gruezi::ha) fn sample_packet() -> HaPacket {
        HaPacket {
            group_id: "cluster-ha".to_owned(),
            sender_id: "node-b".to_owned(),
            receiver_id: "node-a".to_owned(),
            sender_nonce: [0xb5; 16],
            receiver_nonce: [0xa5; 16],
            seq: 42,
            ack_seq: 41,
            role: HaState::Standby,
            authority_source: AdvertAuthoritySource::None,
            resource_state: AdvertResourceState::Absent,
            priority: 110,
            term: 3,
            transfer_id: 0,
            transfer_phase: TransferPhase::None,
        }
    }

    fn key() -> Result<PacketKey> {
        PacketKey::new(TEST_KEY)
    }

    fn hex(text: &str) -> Result<Vec<u8>> {
        let digits = text.as_bytes();
        digits
            .chunks(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| anyhow!("invalid hex"))
            })
            .collect()
    }

    /// Mutate one byte of an encoded packet without indexing.
    fn with_byte(bytes: &[u8], position: usize, value: u8) -> Result<Vec<u8>> {
        let mut copy = bytes.to_vec();
        *copy
            .get_mut(position)
            .ok_or_else(|| anyhow!("position {position} out of range"))? = value;
        Ok(copy)
    }

    fn byte_at(bytes: &[u8], position: usize) -> Result<u8> {
        bytes
            .get(position)
            .copied()
            .ok_or_else(|| anyhow!("position {position} out of range"))
    }

    /// Re-sign a hand-edited packet so a test reaches the framing check it is
    /// aimed at instead of failing on the tag.
    fn resign(bytes: &[u8], key: &PacketKey) -> Result<Vec<u8>> {
        let signed_len = bytes.len().checked_sub(TAG_LEN).context("short packet")?;
        let (signed, _) = bytes.split_at_checked(signed_len).context("split")?;
        let mut out = signed.to_vec();
        out.extend_from_slice(&key.packet_tag(signed));
        Ok(out)
    }

    /// Byte offset of a field that follows the three ids of `sample_packet`.
    fn offset_after_ids(packet: &HaPacket, extra: usize) -> usize {
        HEADER_LEN
            + 3
            + packet.group_id.len()
            + packet.sender_id.len()
            + packet.receiver_id.len()
            + extra
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_test_vectors() -> Result<()> {
        // RFC 4231 section 4.2 (test case 1), 4.3 (test case 2) and 4.7
        // (test case 6, key longer than the block size).
        let cases = [
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, data, expected) in cases {
            let key = PacketKey::new(&key)?;
            let expected = hex(expected)?;
            assert_eq!(key.compute(&[&data]).to_vec(), expected);
            assert!(key.verify(&[&data], &expected));
            let mut wrong = expected.clone();
            if let Some(last) = wrong.last_mut() {
                *last ^= 0x01;
            }
            assert!(!key.verify(&[&data], &wrong));
        }
        Ok(())
    }

    #[test]
    fn tag_is_hmac_over_context_and_every_preceding_byte() -> Result<()> {
        let key = key()?;
        let encoded = sample_packet().encode(&key)?;
        let signed_len = encoded.len() - TAG_LEN;
        let (signed, tag) = encoded.split_at_checked(signed_len).context("split")?;
        let mut message = b"gruezi-ha-v1\0".to_vec();
        message.extend_from_slice(signed);
        assert_eq!(key.compute(&[&message]).as_slice(), tag);
        // Without the domain-separation prefix the tag must not verify.
        assert!(!key.verify(&[signed], tag));
        Ok(())
    }

    #[test]
    fn encodes_and_decodes_packet() -> Result<()> {
        let key = key()?;
        let packet = sample_packet();
        let encoded = packet.encode(&key)?;
        assert!(encoded.len() <= MAX_DATAGRAM_LEN);
        assert_eq!(HaPacket::decode(&encoded, &key), Ok(packet));
        Ok(())
    }

    #[test]
    fn flipping_any_single_byte_before_the_tag_is_rejected() -> Result<()> {
        let key = key()?;
        let encoded = sample_packet().encode(&key)?;
        let signed_len = encoded.len() - TAG_LEN;
        for position in 0..encoded.len() {
            let flipped = with_byte(&encoded, position, byte_at(&encoded, position)? ^ 0x01)?;
            let result = HaPacket::decode(&flipped, &key);
            assert!(result.is_err(), "flip at byte {position} was accepted");
            if position >= signed_len {
                assert_eq!(result, Err(PacketRejection::BadTag));
            }
        }
        // A flip that keeps framing valid (inside an id) fails on the tag.
        assert_eq!(
            HaPacket::decode(&with_byte(&encoded, HEADER_LEN + 1, b'C')?, &key),
            Err(PacketRejection::BadTag)
        );
        Ok(())
    }

    #[test]
    fn wrong_key_is_rejected_as_bad_tag() -> Result<()> {
        let encoded = sample_packet().encode(&PacketKey::new(b"another-key-of-16-bytes")?)?;
        assert_eq!(
            HaPacket::decode(&encoded, &key()?),
            Err(PacketRejection::BadTag)
        );
        Ok(())
    }

    #[test]
    fn rejects_truncation_trailing_bytes_and_oversize() -> Result<()> {
        let key = key()?;
        let encoded = sample_packet().encode(&key)?;
        for len in 0..encoded.len() {
            let truncated = encoded.get(..len).context("prefix")?;
            assert_eq!(
                HaPacket::decode(truncated, &key),
                Err(PacketRejection::Malformed),
                "truncated to {len} bytes"
            );
        }

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            HaPacket::decode(&trailing, &key),
            Err(PacketRejection::Malformed)
        );

        // Trailing bytes inside the signed area with a matching total_len and a
        // valid tag are still rejected: the fields must end exactly at the tag.
        let signed_len = encoded.len() - TAG_LEN;
        let mut padded = encoded.get(..signed_len).context("signed")?.to_vec();
        padded.push(0);
        padded.extend_from_slice(&[0; TAG_LEN]);
        let new_len = u16::try_from(padded.len())?.to_be_bytes();
        let padded = with_byte(&padded, HEADER_LEN - 2, byte_at(&new_len, 0)?)?;
        let padded = with_byte(&padded, HEADER_LEN - 1, byte_at(&new_len, 1)?)?;
        assert_eq!(
            HaPacket::decode(&resign(&padded, &key)?, &key),
            Err(PacketRejection::Malformed)
        );

        let mut oversize = encoded;
        oversize.resize(MAX_DATAGRAM_LEN + 1, 0);
        assert_eq!(
            HaPacket::decode(&oversize, &key),
            Err(PacketRejection::Malformed)
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_magic_unknown_version_kind_flags_and_reserved() -> Result<()> {
        let key = key()?;
        let encoded = sample_packet().encode(&key)?;
        assert_eq!(
            HaPacket::decode(&resign(&with_byte(&encoded, 0, b'X')?, &key)?, &key),
            Err(PacketRejection::Malformed)
        );
        for (position, value) in [(4, 2_u8), (4, 0), (5, 2), (5, 0), (6, 1), (7, 1)] {
            assert_eq!(
                HaPacket::decode(&resign(&with_byte(&encoded, position, value)?, &key)?, &key),
                Err(PacketRejection::Unsupported),
                "byte {position} = {value}"
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_unknown_enum_values_zero_priority_seq_and_nonce() -> Result<()> {
        let key = key()?;
        let packet = sample_packet();
        let encoded = packet.encode(&key)?;
        let role = offset_after_ids(&packet, 32 + 16);
        let transfer_phase = role + 4 + 16;
        for (position, value) in [
            (role, 3_u8),
            (role + 1, 4),
            (role + 2, 6),
            (role + 3, 0),
            (transfer_phase, 5),
        ] {
            assert_eq!(
                HaPacket::decode(&resign(&with_byte(&encoded, position, value)?, &key)?, &key),
                Err(PacketRejection::Unsupported),
                "byte {position} = {value}"
            );
        }

        let zero_seq = HaPacket {
            seq: 0,
            ..sample_packet()
        };
        assert!(zero_seq.encode(&key).is_err());
        let mut raw = encoded;
        let seq_end = offset_after_ids(&packet, 32 + 8);
        for position in (seq_end - 8)..seq_end {
            raw = with_byte(&raw, position, 0)?;
        }
        assert_eq!(
            HaPacket::decode(&resign(&raw, &key)?, &key),
            Err(PacketRejection::Malformed)
        );

        let zero_nonce = HaPacket {
            sender_nonce: ZERO_NONCE,
            ..sample_packet()
        };
        assert!(zero_nonce.encode(&key).is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_id_lengths() -> Result<()> {
        let key = key()?;
        let long = HaPacket {
            group_id: "g".repeat(65),
            ..sample_packet()
        };
        assert!(long.encode(&key).is_err());
        let empty = HaPacket {
            sender_id: String::new(),
            ..sample_packet()
        };
        assert!(empty.encode(&key).is_err());

        let encoded = sample_packet().encode(&key)?;
        // group_id length byte set to 0 and to 65.
        for value in [0_u8, 65] {
            assert_eq!(
                HaPacket::decode(
                    &resign(&with_byte(&encoded, HEADER_LEN, value)?, &key)?,
                    &key
                ),
                Err(PacketRejection::Malformed)
            );
        }
        Ok(())
    }

    #[test]
    fn packet_key_debug_redacts_key_material() -> Result<()> {
        let rendered = format!("{:?}", key()?);
        assert_eq!(rendered, "PacketKey(<redacted>)");
        assert!(PacketKey::new(b"").is_err());
        Ok(())
    }
}
