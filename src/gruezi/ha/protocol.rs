use super::{
    config::{HaAuth, HaRuntimeConfig},
    election::HaState,
};
use anyhow::{Result, bail};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

const PACKET_MAGIC: &[u8; 4] = b"GRHZ";
const MAX_ID_LEN: usize = 64;
const MAX_AUTH_TAG_LEN: usize = 64;

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

pub(super) fn auth_tag(auth: &HaAuth, packet: &HaPacket) -> Vec<u8> {
    match auth {
        HaAuth::None => Vec::new(),
        HaAuth::SharedKey { key } => {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            packet.group_id.hash(&mut hasher);
            packet.node_id.hash(&mut hasher);
            packet.priority.hash(&mut hasher);
            packet.dead_factor.hash(&mut hasher);
            packet.advert_interval_ms.hash(&mut hasher);
            packet.state.as_u8().hash(&mut hasher);
            packet.sequence.hash(&mut hasher);
            hasher.finish().to_be_bytes().to_vec()
        }
    }
}

pub(super) fn verify_auth_tag(runtime: &HaRuntimeConfig, packet: &HaPacket) -> bool {
    auth_tag(&runtime.auth, packet) == packet.auth_tag
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
    use super::HaPacket;
    use crate::gruezi::ha::HaState;
    use anyhow::Result;

    #[test]
    fn encodes_and_decodes_packet() -> Result<()> {
        let packet = HaPacket {
            protocol_version: 1,
            state: HaState::Standby,
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
    fn rejects_truncated_packet() -> Result<()> {
        let packet = HaPacket {
            protocol_version: 1,
            state: HaState::Standby,
            priority: 100,
            dead_factor: 3,
            advert_interval_ms: 1_000,
            sequence: 1,
            node_id: "node-a".to_owned(),
            group_id: "cluster-ha".to_owned(),
            auth_tag: vec![],
        };
        let encoded = packet.encode()?;

        let truncated = encoded
            .get(..8)
            .ok_or_else(|| anyhow::anyhow!("encoded packet shorter than expected"))?;
        assert!(HaPacket::decode(truncated).is_err());

        Ok(())
    }
}
