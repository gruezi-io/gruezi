use crate::{
    config::{Config, HaAuthMode, Mode},
    gruezi::hooks::HaHooks,
};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

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
    pub hooks: HaHooks,
    pub ip_command: String,
    pub arping_command: String,
    pub ndsend_command: String,
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
            hooks: HaHooks {
                on_promote: config.ha.hooks.on_promote.clone(),
                on_demote: config.ha.hooks.on_demote.clone(),
                on_backup: config.ha.hooks.on_backup.clone(),
                on_fault: config.ha.hooks.on_fault.clone(),
                timeout_ms: config.ha.hooks.timeout_ms,
            },
            ip_command: "ip".to_owned(),
            arping_command: "arping".to_owned(),
            ndsend_command: "ndsend".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HaAuth, HaRuntimeConfig};
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
        let observed_at = Instant::now();

        assert_eq!(runtime.advert_interval(), Duration::from_secs(1));
        assert_eq!(runtime.dead_timeout(), Duration::from_secs(3));
        assert_eq!(runtime.hold_down(), Duration::from_secs(3));
        assert_eq!(runtime.next_advert_delay(50), Duration::from_millis(950));
        assert!(runtime.follower_deadline(observed_at) >= observed_at + Duration::from_secs(6));

        Ok(())
    }
}
