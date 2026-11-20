use anyhow::Result;
use gruezi::config::{Config, FailurePolicy, WatchdogMode};
use std::path::Path;

#[test]
fn strict_mariadb_examples_parse_through_real_config_parser() -> Result<()> {
    for node in ["a", "b"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join(format!("ha-mariadb-node-{node}.yaml"));
        let config = Config::from_path(&path)?;
        assert_eq!(config.ha.failure_policy, Some(FailurePolicy::Strict));
        assert_eq!(
            config.ha.lifecycle.as_ref().map(|value| value.timeout_ms),
            Some(2500)
        );
        assert_eq!(config.ha.watchdog.mode, WatchdogMode::Required);
        assert_eq!(config.ha.watchdog.timeout_ms, 30_000);
        println!(
            "parsed node={node} policy=strict activate={} deactivate={} lifecycle_timeout_ms={} watchdog_mode=required watchdog_timeout_ms={}",
            config
                .ha
                .lifecycle
                .as_ref()
                .map_or("", |value| value.activate_command.as_str()),
            config
                .ha
                .lifecycle
                .as_ref()
                .map_or("", |value| value.deactivate_command.as_str()),
            config
                .ha
                .lifecycle
                .as_ref()
                .map_or(0, |value| value.timeout_ms),
            config.ha.watchdog.timeout_ms
        );
    }
    Ok(())
}
