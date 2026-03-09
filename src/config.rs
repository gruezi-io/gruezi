use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{fs, path::Path};

pub const DEFAULT_HA_BIND: &str = "0.0.0.0:9375";
pub const DEFAULT_KV_CLIENT_LISTEN: &str = "0.0.0.0:9376";
pub const DEFAULT_KV_PEER_LISTEN: &str = "0.0.0.0:9377";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Ha,
    Kv,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub ha: HaConfig,
    #[serde(default)]
    pub kv: KvConfig,
}

impl Config {
    /// Parse and validate a configuration from YAML text.
    ///
    /// # Errors
    ///
    /// Returns an error if the YAML cannot be parsed or the config is invalid.
    pub fn from_yaml_str(input: &str) -> Result<Self> {
        let config: Self = serde_yaml::from_str(input).context("failed to parse YAML config")?;
        config.validate()?;
        Ok(config)
    }

    /// Load and validate a configuration from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, parsed, or validated.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;

        Self::from_yaml_str(&raw).with_context(|| format!("invalid config file {}", path.display()))
    }

    /// Validate mode-specific requirements.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is inconsistent or incomplete.
    pub fn validate(&self) -> Result<()> {
        match self.mode {
            Mode::Ha => self.validate_ha(),
            Mode::Kv => self.validate_kv(),
        }
    }

    fn validate_ha(&self) -> Result<()> {
        let node_id = self
            .node
            .id
            .as_deref()
            .map(str::trim)
            .filter(|node_id| !node_id.is_empty());

        if node_id.is_none() {
            bail!("mode 'ha' requires node.id");
        }

        if self.ha.interface.trim().is_empty() {
            bail!("mode 'ha' requires ha.interface");
        }

        if self.ha.group_id.trim().is_empty() {
            bail!("mode 'ha' requires ha.group_id");
        }

        if self.ha.addresses.is_empty() {
            bail!("mode 'ha' requires at least one address in ha.addresses");
        }

        if self
            .ha
            .addresses
            .iter()
            .any(|address| address.trim().is_empty())
        {
            bail!("mode 'ha' does not allow empty entries in ha.addresses");
        }

        let peer = self
            .ha
            .peer
            .as_deref()
            .map(str::trim)
            .filter(|peer| !peer.is_empty());

        if peer.is_none() {
            bail!("mode 'ha' requires ha.peer");
        }

        if self.ha.priority == 0 {
            bail!("mode 'ha' requires ha.priority to be greater than 0");
        }

        if self.ha.advert_interval_ms == 0 {
            bail!("mode 'ha' requires ha.advert_interval_ms to be greater than 0");
        }

        if self.ha.protocol_version == 0 {
            bail!("mode 'ha' requires ha.protocol_version to be greater than 0");
        }

        if self.ha.dead_factor < 2 {
            bail!("mode 'ha' requires ha.dead_factor to be at least 2");
        }

        if self.ha.jitter_ms >= self.ha.advert_interval_ms {
            bail!("mode 'ha' requires ha.jitter_ms to be smaller than ha.advert_interval_ms");
        }

        match self.ha.auth.mode {
            HaAuthMode::None => {
                if self.ha.auth.key.is_some() {
                    bail!("mode 'ha' does not allow ha.auth.key when ha.auth.mode is 'none'");
                }
            }
            HaAuthMode::SharedKey => {
                let has_key = self
                    .ha
                    .auth
                    .key
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|key| !key.is_empty());

                if !has_key {
                    bail!("mode 'ha' requires ha.auth.key when ha.auth.mode is 'shared_key'");
                }
            }
        }

        Ok(())
    }

    fn validate_kv(&self) -> Result<()> {
        if self.kv.data_dir.trim().is_empty() {
            bail!("mode 'kv' requires kv.data_dir");
        }

        if self.kv.initial_cluster.len() < 3 {
            bail!("mode 'kv' requires at least 3 entries in kv.initial_cluster");
        }

        if self
            .kv
            .initial_cluster
            .iter()
            .any(|member| member.trim().is_empty())
        {
            bail!("mode 'kv' does not allow empty entries in kv.initial_cluster");
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    pub id: Option<String>,
    pub listen: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HaConfig {
    pub bind: String,
    pub interface: String,
    pub addresses: Vec<String>,
    pub peer: Option<String>,
    pub group_id: String,
    pub protocol_version: u8,
    pub priority: u8,
    pub preempt: bool,
    pub advert_interval_ms: u64,
    pub dead_factor: u8,
    pub hold_down_ms: u64,
    pub jitter_ms: u64,
    pub auth: HaAuthConfig,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_HA_BIND.to_owned(),
            interface: String::new(),
            addresses: Vec::new(),
            peer: None,
            group_id: String::new(),
            protocol_version: 1,
            priority: 100,
            preempt: true,
            advert_interval_ms: 1_000,
            dead_factor: 3,
            hold_down_ms: 3_000,
            jitter_ms: 100,
            auth: HaAuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaAuthMode {
    None,
    SharedKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HaAuthConfig {
    pub mode: HaAuthMode,
    pub key: Option<String>,
}

impl Default for HaAuthConfig {
    fn default() -> Self {
        Self {
            mode: HaAuthMode::None,
            key: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KvRole {
    Voter,
    Witness,
    Learner,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KvConfig {
    pub role: KvRole,
    pub listen_client: String,
    pub listen_peer: String,
    pub data_dir: String,
    pub initial_cluster: Vec<String>,
}

impl Default for KvConfig {
    fn default() -> Self {
        Self {
            role: KvRole::Voter,
            listen_client: DEFAULT_KV_CLIENT_LISTEN.to_owned(),
            listen_peer: DEFAULT_KV_PEER_LISTEN.to_owned(),
            data_dir: String::new(),
            initial_cluster: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, DEFAULT_HA_BIND, DEFAULT_KV_CLIENT_LISTEN, DEFAULT_KV_PEER_LISTEN, HaAuthMode,
        KvRole, Mode,
    };
    use anyhow::Result;

    #[test]
    fn parses_valid_ha_config() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
  listen:
    - 10.0.0.1:7000
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:7000
  protocol_version: 1
  priority: 120
  preempt: true
  advert_interval_ms: 500
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 50
  auth:
    mode: shared_key
    key: super-secret
",
        )?;

        assert!(matches!(config.mode, Mode::Ha));
        assert_eq!(config.node.id.as_deref(), Some("node-a"));
        assert_eq!(config.ha.bind, DEFAULT_HA_BIND);
        assert_eq!(config.ha.group_id, "cluster-ha");
        assert_eq!(config.ha.protocol_version, 1);
        assert_eq!(config.ha.priority, 120);
        assert_eq!(config.ha.addresses.len(), 1);
        assert_eq!(config.ha.advert_interval_ms, 500);
        assert_eq!(config.ha.dead_factor, 3);
        assert_eq!(config.ha.hold_down_ms, 3_000);
        assert_eq!(config.ha.jitter_ms, 50);
        assert!(matches!(config.ha.auth.mode, HaAuthMode::SharedKey));

        Ok(())
    }

    #[test]
    fn rejects_invalid_ha_config() {
        let result = Config::from_yaml_str(
            r"
mode: ha
ha:
  interface: eth0
  addresses:
    - 10.0.0.10/24
",
        );

        assert!(result.is_err());
    }

    #[test]
    fn rejects_shared_key_auth_without_key() {
        let result = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  addresses:
    - 10.0.0.10/24
  group_id: cluster-ha
  peer: 10.0.0.2:7000
  auth:
    mode: shared_key
",
        );

        assert!(result.is_err());
    }

    #[test]
    fn parses_valid_kv_config() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: kv
kv:
  role: witness
  data_dir: /var/lib/gruezi
  initial_cluster:
    - node-a=http://10.0.0.1:2380
    - node-b=http://10.0.0.2:2380
    - witness=http://10.0.0.3:2380
",
        )?;

        assert!(matches!(config.mode, Mode::Kv));
        assert!(matches!(config.kv.role, KvRole::Witness));
        assert_eq!(config.kv.listen_client, DEFAULT_KV_CLIENT_LISTEN);
        assert_eq!(config.kv.listen_peer, DEFAULT_KV_PEER_LISTEN);
        assert_eq!(config.kv.initial_cluster.len(), 3);

        Ok(())
    }

    #[test]
    fn rejects_small_kv_cluster() {
        let result = Config::from_yaml_str(
            r"
mode: kv
kv:
  role: voter
  data_dir: /var/lib/gruezi
  initial_cluster:
    - node-a=http://10.0.0.1:2380
    - node-b=http://10.0.0.2:2380
",
        );

        assert!(result.is_err());
    }
}
