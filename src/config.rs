use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{fs, path::Path};

pub const DEFAULT_HA_BIND: &str = "0.0.0.0:9375";
pub const DEFAULT_API_PORT: u16 = 9376;
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
        let config: Self = serde_saphyr::from_str(input).context("failed to parse YAML config")?;
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

        if !self.ha.addresses.is_empty() && self.ha.interface.trim().is_empty() {
            bail!("mode 'ha' requires ha.interface");
        }

        if self.ha.group_id.trim().is_empty() {
            bail!("mode 'ha' requires ha.group_id");
        }

        if self.ha.failure_policy.is_none() {
            bail!("mode 'ha' requires ha.failure_policy");
        }

        let peer_id = self.ha.peer_id.as_deref().unwrap_or_default();
        if peer_id.trim().is_empty() || peer_id.len() > 64 {
            bail!("ha.peer_id must contain 1..=64 bytes");
        }
        if Some(peer_id) == node_id {
            bail!("ha.peer_id must differ from node.id");
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

        if self.ha.dead_factor < 2 {
            bail!("mode 'ha' requires ha.dead_factor to be at least 2");
        }

        if self.ha.jitter_ms >= self.ha.advert_interval_ms {
            bail!("mode 'ha' requires ha.jitter_ms to be smaller than ha.advert_interval_ms");
        }

        if self.ha.hooks.timeout_ms == 0 {
            bail!("mode 'ha' requires ha.hooks.timeout_ms to be greater than 0");
        }

        validate_hook_path(self.ha.hooks.on_promote.as_deref(), "ha.hooks.on_promote")?;
        validate_hook_path(self.ha.hooks.on_demote.as_deref(), "ha.hooks.on_demote")?;
        validate_hook_path(self.ha.hooks.on_backup.as_deref(), "ha.hooks.on_backup")?;
        validate_hook_path(self.ha.hooks.on_fault.as_deref(), "ha.hooks.on_fault")?;

        if self.ha.auth.key.as_ref().is_none_or(|key| key.len() < 16) {
            bail!("ha.auth.key must contain at least 16 bytes");
        }
        if !(500..=60_000).contains(&self.ha.withdraw_timeout_ms) {
            bail!("ha.withdraw_timeout_ms must be in 500..=60000");
        }
        if self.ha.command_timeout_ms == 0
            || self.ha.command_timeout_ms >= self.ha.withdraw_timeout_ms
        {
            bail!("ha.command_timeout_ms must be positive and smaller than ha.withdraw_timeout_ms");
        }
        self.validate_watchdog()?;
        self.validate_authority()?;

        Ok(())
    }

    fn validate_watchdog(&self) -> Result<()> {
        let watchdog = &self.ha.watchdog;
        if watchdog.device.trim().is_empty() {
            bail!("ha.watchdog.device cannot be empty");
        }
        if watchdog.timeout_ms == 0 {
            bail!("ha.watchdog.timeout_ms must be positive");
        }
        if watchdog.safety_margin_ms == 0 {
            bail!("ha.watchdog.safety_margin_ms must be positive");
        }
        if watchdog.mode != WatchdogMode::Off {
            #[cfg(not(target_os = "linux"))]
            bail!("ha.watchdog.mode requires Linux");

            #[cfg(target_os = "linux")]
            {
                let budget = u128::from(self.ha.advert_interval_ms)
                    + u128::from(self.ha.command_timeout_ms)
                    + u128::from(self.ha.withdraw_timeout_ms)
                    + u128::from(watchdog.safety_margin_ms);
                if budget >= u128::from(watchdog.timeout_ms) {
                    bail!(
                        "ha.watchdog.timeout_ms must exceed ha.advert_interval_ms + ha.command_timeout_ms + ha.withdraw_timeout_ms + ha.watchdog.safety_margin_ms"
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_authority(&self) -> Result<()> {
        match (&self.ha.failure_policy, &self.ha.authority) {
            (Some(FailurePolicy::External), Some(authority)) => {
                if authority.check_command.trim().is_empty() {
                    bail!("ha.authority.check_command is required");
                }
                validate_hook_path(
                    authority.fence_command.as_deref(),
                    "ha.authority.fence_command",
                )?;
                if authority.interval_ms == Some(0) {
                    bail!("ha.authority.interval_ms must be positive");
                }
                if authority.grant_ttl_ms == 0 {
                    bail!("ha.authority.grant_ttl_ms must be positive");
                }
                // The grant lifetime is grant_ttl_ms minus this sum; a non-positive lifetime
                // makes every check result expire on arrival, leaving the node stuck pending.
                let lifetime_floor =
                    self.ha.withdraw_timeout_ms + u64::from(self.ha.watchdog.safety_margin_ms);
                if authority.grant_ttl_ms <= lifetime_floor {
                    bail!(
                        "ha.authority.grant_ttl_ms must be greater than ha.withdraw_timeout_ms + ha.watchdog.safety_margin_ms"
                    );
                }
            }
            (Some(FailurePolicy::External), None) => {
                bail!("ha.authority is required for ha.failure_policy external");
            }
            (_, Some(_)) => bail!("ha.authority is only allowed for ha.failure_policy external"),
            (_, None) => {}
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

fn validate_hook_path(path: Option<&str>, field_name: &str) -> Result<()> {
    if path.is_some_and(|path| path.trim().is_empty()) {
        bail!("mode 'ha' does not allow an empty value for {field_name}");
    }

    Ok(())
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
    pub peer_id: Option<String>,
    pub group_id: String,
    pub failure_policy: Option<FailurePolicy>,
    pub priority: u8,
    pub preempt: bool,
    pub advert_interval_ms: u64,
    pub dead_factor: u8,
    pub hold_down_ms: u64,
    pub jitter_ms: u64,
    pub auth: HaAuthConfig,
    pub hooks: HaHooksConfig,
    pub withdraw_timeout_ms: u64,
    pub command_timeout_ms: u64,
    pub watchdog: WatchdogConfig,
    pub authority: Option<AuthorityConfig>,
}

impl Default for HaConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_HA_BIND.to_owned(),
            interface: String::new(),
            addresses: Vec::new(),
            peer: None,
            peer_id: None,
            group_id: String::new(),
            failure_policy: None,
            priority: 100,
            // Prefer stable failover over automatic failback for application VIPs.
            // This avoids a second VIP move when the original preferred node recovers.
            preempt: false,
            advert_interval_ms: 1_000,
            dead_factor: 3,
            hold_down_ms: 3_000,
            jitter_ms: 100,
            auth: HaAuthConfig::default(),
            hooks: HaHooksConfig::default(),
            withdraw_timeout_ms: 5_000,
            command_timeout_ms: 3_000,
            watchdog: WatchdogConfig::default(),
            authority: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    Strict,
    Available,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HaAuthConfig {
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchdogMode {
    #[default]
    Off,
    Automatic,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchdogNowayout {
    #[default]
    Reject,
    Reboot,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchdogConfig {
    pub mode: WatchdogMode,
    pub device: String,
    pub timeout_ms: u32,
    pub safety_margin_ms: u32,
    pub nowayout: WatchdogNowayout,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            mode: WatchdogMode::Off,
            device: "/dev/watchdog0".to_owned(),
            timeout_ms: 15_000,
            safety_margin_ms: 2_000,
            nowayout: WatchdogNowayout::Reject,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthorityConfig {
    pub check_command: String,
    pub fence_command: Option<String>,
    /// None uses the local advertisement interval, resolved at the runtime boundary.
    pub interval_ms: Option<u64>,
    pub grant_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HaHooksConfig {
    pub on_promote: Option<String>,
    pub on_demote: Option<String>,
    pub on_backup: Option<String>,
    pub on_fault: Option<String>,
    pub timeout_ms: u64,
}

impl HaHooksConfig {
    #[must_use]
    pub fn default_timeout_ms() -> u64 {
        5_000
    }
}

impl Default for HaHooksConfig {
    fn default() -> Self {
        Self {
            on_promote: None,
            on_demote: None,
            on_backup: None,
            on_fault: None,
            timeout_ms: Self::default_timeout_ms(),
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
        Config, DEFAULT_HA_BIND, DEFAULT_KV_CLIENT_LISTEN, DEFAULT_KV_PEER_LISTEN, KvRole, Mode,
    };
    use anyhow::Result;
    use std::path::Path;

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
  failure_policy: strict
  peer_id: node-b
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 10.0.0.2:7000
  priority: 120
  preempt: true
  advert_interval_ms: 500
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 50
  auth:
    key: super-secret-for-tests
",
        )?;

        assert!(matches!(config.mode, Mode::Ha));
        assert_eq!(config.node.id.as_deref(), Some("node-a"));
        assert_eq!(config.ha.bind, DEFAULT_HA_BIND);
        assert_eq!(config.ha.group_id, "cluster-ha");
        assert_eq!(config.ha.priority, 120);
        assert_eq!(config.ha.addresses.len(), 1);
        assert_eq!(config.ha.advert_interval_ms, 500);
        assert_eq!(config.ha.dead_factor, 3);
        assert_eq!(config.ha.hold_down_ms, 3_000);
        assert_eq!(config.ha.jitter_ms, 50);
        assert_eq!(
            config.ha.auth.key.as_deref(),
            Some("super-secret-for-tests")
        );

        Ok(())
    }

    #[test]
    fn rejects_invalid_ha_config() -> Result<()> {
        let result = Config::from_yaml_str(
            r"
mode: ha
ha:
  failure_policy: strict
  peer_id: node-b
  interface: eth0
  addresses:
    - 10.0.0.10/24
",
        );

        let error = result
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected missing node.id"))?;
        assert!(format!("{error:#}").contains("node.id"));
        Ok(())
    }

    #[test]
    fn rejects_shared_key_auth_without_key() -> Result<()> {
        let result = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  failure_policy: strict
  peer_id: node-b
  interface: eth0
  addresses:
    - 10.0.0.10/24
  group_id: cluster-ha
  peer: 10.0.0.2:7000
  auth: {}
",
        );

        let error = result
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected missing key"))?;
        assert!(format!("{error:#}").contains("ha.auth.key"));
        Ok(())
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

    #[test]
    fn ha_defaults_disable_preemption() {
        let defaults = super::HaConfig::default();
        assert!(!defaults.preempt);
    }

    const MINIMAL_HA: &str = "
mode: ha
node:
  id: node-a
ha:
  failure_policy: strict
  peer_id: node-b
  group_id: test
  peer: 127.0.0.1:9
  auth:
    key: '  raw-key-bytes  '
";

    fn rejects_change(old: &str, new: &str, field: &str) -> Result<()> {
        let input = MINIMAL_HA.replace(old, new);
        let error = Config::from_yaml_str(&input)
            .err()
            .ok_or_else(|| anyhow::anyhow!("accepted invalid {field}: {input}"))?;
        let message = format!("{error:#}");
        assert!(message.contains(field), "{field}: {message}");
        Ok(())
    }

    macro_rules! invalid_config {
        ($name:ident, $old:expr, $new:expr, $field:expr) => {
            #[test]
            fn $name() -> Result<()> {
                rejects_change($old, $new, $field)
            }
        };
    }

    invalid_config!(
        missing_policy,
        "  failure_policy: strict\n",
        "",
        "ha.failure_policy"
    );
    invalid_config!(
        unknown_policy,
        "failure_policy: strict",
        "failure_policy: unsafe",
        "failure_policy"
    );
    invalid_config!(
        missing_peer_identity,
        "  peer_id: node-b\n",
        "",
        "ha.peer_id"
    );
    invalid_config!(
        empty_peer_identity,
        "peer_id: node-b",
        "peer_id: ''",
        "ha.peer_id"
    );
    invalid_config!(
        long_peer_identity,
        "peer_id: node-b",
        &format!("peer_id: {}", "x".repeat(65)),
        "ha.peer_id"
    );
    invalid_config!(
        duplicate_peer_identity,
        "peer_id: node-b",
        "peer_id: node-a",
        "ha.peer_id"
    );
    invalid_config!(
        addresses_require_interface,
        "  group_id: test",
        "  addresses: [10.0.0.1/24]\n  group_id: test",
        "ha.interface"
    );
    invalid_config!(
        empty_address_rejected,
        "  group_id: test",
        "  addresses: ['']\n  interface: eth0\n  group_id: test",
        "ha.addresses"
    );
    invalid_config!(
        missing_key,
        "  auth:\n    key: '  raw-key-bytes  '\n",
        "  auth: {}\n",
        "ha.auth.key"
    );
    invalid_config!(
        short_key,
        "'  raw-key-bytes  '",
        "123456789012345",
        "ha.auth.key"
    );
    invalid_config!(
        old_auth_setting_rejected,
        "  auth:",
        "  auth:\n    mode: shared_key",
        "mode"
    );
    invalid_config!(
        unknown_ha_field_rejected,
        "  group_id: test",
        "  unknown_setting: 1\n  group_id: test",
        "unknown_setting"
    );
    invalid_config!(
        withdraw_below_minimum,
        "  group_id: test",
        "  withdraw_timeout_ms: 499\n  group_id: test",
        "ha.withdraw_timeout_ms"
    );
    invalid_config!(
        withdraw_above_maximum,
        "  group_id: test",
        "  withdraw_timeout_ms: 60001\n  group_id: test",
        "ha.withdraw_timeout_ms"
    );
    invalid_config!(
        zero_command_timeout,
        "  group_id: test",
        "  command_timeout_ms: 0\n  group_id: test",
        "ha.command_timeout_ms"
    );
    invalid_config!(
        equal_command_timeout,
        "  group_id: test",
        "  command_timeout_ms: 5000\n  group_id: test",
        "ha.command_timeout_ms"
    );
    invalid_config!(
        larger_command_timeout,
        "  group_id: test",
        "  command_timeout_ms: 5001\n  group_id: test",
        "ha.command_timeout_ms"
    );
    invalid_config!(
        external_requires_authority,
        "failure_policy: strict",
        "failure_policy: external",
        "ha.authority"
    );
    invalid_config!(
        strict_forbids_authority,
        "  group_id: test",
        "  authority: {}\n  group_id: test",
        "ha.authority"
    );
    invalid_config!(
        available_forbids_authority,
        "  failure_policy: strict",
        "  failure_policy: available\n  authority: {}",
        "ha.authority"
    );
    invalid_config!(
        authority_requires_check_command,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {grant_ttl_ms: 10000}",
        "ha.authority.check_command"
    );
    invalid_config!(
        authority_rejects_empty_check_command,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: ' ', grant_ttl_ms: 10000}",
        "ha.authority.check_command"
    );
    invalid_config!(
        authority_rejects_empty_fence_command,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: /check, fence_command: '', grant_ttl_ms: 10000}",
        "ha.authority.fence_command"
    );
    invalid_config!(
        authority_requires_grant_ttl,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: /check}",
        "ha.authority.grant_ttl_ms"
    );
    invalid_config!(
        authority_rejects_zero_grant_ttl,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: /check, grant_ttl_ms: 0}",
        "ha.authority.grant_ttl_ms"
    );
    invalid_config!(
        authority_rejects_zero_interval,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: /check, interval_ms: 0, grant_ttl_ms: 10000}",
        "ha.authority.interval_ms"
    );
    invalid_config!(
        authority_rejects_grant_ttl_equal_to_lifetime_floor,
        "  failure_policy: strict",
        "  failure_policy: external\n  authority: {check_command: /check, grant_ttl_ms: 7000}",
        "ha.authority.grant_ttl_ms"
    );
    invalid_config!(
        authority_rejects_grant_ttl_below_lifetime_floor_with_custom_margin,
        "  failure_policy: strict",
        "  failure_policy: external\n  watchdog: {safety_margin_ms: 6000}\n  authority: {check_command: /check, grant_ttl_ms: 10000}",
        "ha.authority.grant_ttl_ms"
    );

    #[test]
    fn authority_accepts_grant_ttl_above_lifetime_floor() -> Result<()> {
        Config::from_yaml_str(&MINIMAL_HA.replace(
            "failure_policy: strict",
            "failure_policy: external\n  authority: {check_command: /check, grant_ttl_ms: 7001}",
        ))?;
        Ok(())
    }

    invalid_config!(
        watchdog_rejects_unknown_mode,
        "  group_id: test",
        "  watchdog: {mode: invalid}\n  group_id: test",
        "mode"
    );
    invalid_config!(
        watchdog_rejects_empty_device,
        "  group_id: test",
        "  watchdog: {device: ''}\n  group_id: test",
        "ha.watchdog.device"
    );
    invalid_config!(
        watchdog_rejects_zero_timeout,
        "  group_id: test",
        "  watchdog: {timeout_ms: 0}\n  group_id: test",
        "ha.watchdog.timeout_ms"
    );
    invalid_config!(
        watchdog_rejects_zero_margin,
        "  group_id: test",
        "  watchdog: {safety_margin_ms: 0}\n  group_id: test",
        "ha.watchdog.safety_margin_ms"
    );
    invalid_config!(
        watchdog_rejects_unknown_nowayout,
        "  group_id: test",
        "  watchdog: {nowayout: invalid}\n  group_id: test",
        "nowayout"
    );

    #[test]
    fn optional_vip_and_defaults_load() -> Result<()> {
        for fields in [
            "",
            "  addresses: []\n",
            "  addresses: []\n  interface: eth0\n",
        ] {
            let config =
                Config::from_yaml_str(&MINIMAL_HA.replace("ha:\n", &format!("ha:\n{fields}")))?;
            assert_eq!(config.ha.addresses, Vec::<String>::new());
            assert_eq!(config.ha.withdraw_timeout_ms, 5_000);
            assert_eq!(config.ha.command_timeout_ms, 3_000);
            assert_eq!(config.ha.watchdog, super::WatchdogConfig::default());
            let runtime = crate::gruezi::ha::HaRuntimeConfig::try_from(&config)?;
            assert_eq!(runtime.peer_id, "node-b");
            assert_eq!(runtime.failure_policy, super::FailurePolicy::Strict);
            assert_eq!(
                runtime.auth,
                crate::gruezi::ha::HaAuth::SharedKey {
                    key: "  raw-key-bytes  ".to_owned()
                }
            );
        }
        Ok(())
    }

    #[test]
    fn peer_identity_byte_boundaries_load() -> Result<()> {
        for peer in ["x".to_owned(), "x".repeat(64), "é".repeat(32)] {
            Config::from_yaml_str(
                &MINIMAL_HA.replace("peer_id: node-b", &format!("peer_id: {peer}")),
            )?;
        }
        rejects_change(
            "peer_id: node-b",
            &format!("peer_id: {}", "é".repeat(33)),
            "ha.peer_id",
        )
    }

    #[test]
    fn key_length_uses_raw_utf8_bytes() -> Result<()> {
        for key in ["x".repeat(16), "é".repeat(8)] {
            Config::from_yaml_str(&MINIMAL_HA.replace("'  raw-key-bytes  '", &key))?;
        }
        rejects_change("'  raw-key-bytes  '", &"é".repeat(7), "ha.auth.key")
    }

    #[test]
    fn removed_wire_version_setting_is_rejected() -> Result<()> {
        let field = ["protocol", "_version"].concat();
        rejects_change(
            "  group_id: test",
            &format!("  {field}: 1\n  group_id: test"),
            &field,
        )
    }

    #[test]
    fn timeout_boundaries_load() -> Result<()> {
        for (withdraw, command) in [(500, 499), (60_000, 59_999)] {
            let fields = format!(
                "ha:\n  withdraw_timeout_ms: {withdraw}\n  command_timeout_ms: {command}\n"
            );
            let config = Config::from_yaml_str(&MINIMAL_HA.replace("ha:\n", &fields))?;
            let runtime = crate::gruezi::ha::HaRuntimeConfig::try_from(&config)?;
            assert_eq!(runtime.withdraw_timeout_ms, withdraw);
            assert_eq!(runtime.command_timeout_ms, command);
        }
        Ok(())
    }

    #[test]
    fn all_policies_and_external_authority_load() -> Result<()> {
        Config::from_yaml_str(&MINIMAL_HA.replace("strict", "available"))?;
        for optional in ["", ", fence_command: /fence, interval_ms: 1500"] {
            let fields = format!(
                "failure_policy: external\n  authority: {{check_command: /check, grant_ttl_ms: 10000{optional}}}"
            );
            let config =
                Config::from_yaml_str(&MINIMAL_HA.replace("failure_policy: strict", &fields))?;
            let runtime = crate::gruezi::ha::HaRuntimeConfig::try_from(&config)?;
            let authority = runtime
                .authority
                .ok_or_else(|| anyhow::anyhow!("missing authority"))?;
            assert_eq!(authority.check_command, "/check");
            assert_eq!(authority.grant_ttl_ms, 10_000);
            assert_eq!(
                authority.interval_ms,
                Some(if optional.is_empty() { 1000 } else { 1500 })
            );
            assert_eq!(
                authority.fence_command.as_deref(),
                if optional.is_empty() {
                    None
                } else {
                    Some("/fence")
                }
            );
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn watchdog_modes_and_timing_boundaries_load() -> Result<()> {
        for mode in ["off", "automatic", "required"] {
            for nowayout in ["reject", "reboot"] {
                let fields = format!("ha:\n  watchdog: {{mode: {mode}, nowayout: {nowayout}}}\n");
                let config = Config::from_yaml_str(&MINIMAL_HA.replace("ha:\n", &fields))?;
                let runtime = crate::gruezi::ha::HaRuntimeConfig::try_from(&config)?;
                assert_eq!(runtime.watchdog, config.ha.watchdog);
            }
        }
        Config::from_yaml_str(&MINIMAL_HA.replace(
            "ha:\n",
            "ha:\n  watchdog: {mode: required, device: /test/watchdog, timeout_ms: 11001}\n",
        ))?;
        rejects_change(
            "  group_id: test",
            "  watchdog: {mode: required, timeout_ms: 11000}\n  group_id: test",
            "ha.watchdog.timeout_ms",
        )?;
        rejects_change(
            "  group_id: test",
            "  watchdog: {mode: automatic, safety_margin_ms: 6000}\n  group_id: test",
            "ha.watchdog.safety_margin_ms",
        )?;
        rejects_change(
            "  group_id: test",
            "  advert_interval_ms: 18446744073709551615\n  watchdog: {mode: required}\n  group_id: test",
            "ha.watchdog.timeout_ms",
        )
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_rejects_enabled_watchdog() -> Result<()> {
        for mode in ["automatic", "required"] {
            rejects_change(
                "  group_id: test",
                &format!("  watchdog: {{mode: {mode}}}\n  group_id: test"),
                "ha.watchdog.mode",
            )?;
        }
        Ok(())
    }

    #[test]
    fn loads_every_yaml_example() -> Result<()> {
        for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"))? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "yaml")
            {
                Config::from_path(path)?;
            }
        }
        Ok(())
    }
}
