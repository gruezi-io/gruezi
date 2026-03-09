use crate::config::{Config, Mode};
use crate::gruezi;
use anyhow::{Context, Result};
use std::{env, path::Path};

const DEFAULT_CONFIG_PATH: &str = "/etc/gruezi/gruezi.yaml";

/// Execute the start action
///
/// # Errors
///
/// Returns an error if the service fails to start
pub async fn run(
    config_path: Option<&str>,
    bind: &str,
    peers: Option<&str>,
    node_id: Option<&str>,
    verbose: bool,
) -> Result<()> {
    if let Some((config, path)) = load_config(config_path)? {
        let path = path.display().to_string();

        if verbose {
            println!("Loaded config: {path}");
            match config.mode {
                Mode::Ha => {
                    println!("Mode: ha");
                    println!("Bind: {}", config.ha.bind);
                    println!("Interface: {}", config.ha.interface);
                    println!("Group ID: {}", config.ha.group_id);
                    println!("Peer: {}", config.ha.peer.as_deref().unwrap_or("unknown"));
                    println!("Protocol version: {}", config.ha.protocol_version);
                    println!("Advert interval: {}ms", config.ha.advert_interval_ms);
                    println!("Dead factor: {}", config.ha.dead_factor);
                }
                Mode::Kv => {
                    println!("Mode: kv");
                    println!("Client listen: {}", config.kv.listen_client);
                    println!("Peer listen: {}", config.kv.listen_peer);
                    println!("Data dir: {}", config.kv.data_dir);
                    println!("Members: {}", config.kv.initial_cluster.len());
                }
            }
        }

        println!("Starting gruezi using config {path}");
        gruezi::start::start_service_with_config(&config)
            .await
            .with_context(|| format!("failed to start service using config {path}"))?;
        println!("Gruezi service stopped using config {path}");
        return Ok(());
    }

    if verbose {
        println!("Bind address: {bind}");
        if let Some(p) = peers {
            println!("Peers: {p}");
        }
        if let Some(id) = node_id {
            println!("Node ID: {id}");
        }
    }

    // Call the core gruezi service startup logic
    println!("Starting gruezi on {bind}");
    gruezi::start::start_service(bind, peers, node_id)?;

    println!("Gruezi service stopped on {bind}");

    Ok(())
}

fn load_config(config_path: Option<&str>) -> Result<Option<(Config, std::path::PathBuf)>> {
    let resolved_path = resolve_config_path(
        config_path,
        env::var_os("GRUEZI_CONFIG"),
        DEFAULT_CONFIG_PATH,
    );

    resolved_path
        .map(|path| Config::from_path(&path).map(|config| (config, path)))
        .transpose()
}

fn resolve_config_path(
    cli_path: Option<&str>,
    env_path: Option<std::ffi::OsString>,
    default_path: &str,
) -> Option<std::path::PathBuf> {
    cli_path
        .map(Path::new)
        .map(std::path::PathBuf::from)
        .or_else(|| env_path.map(std::path::PathBuf::from))
        .or_else(|| {
            let default = Path::new(default_path);
            default.exists().then(|| default.to_path_buf())
        })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_CONFIG_PATH, resolve_config_path};
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn prefers_cli_config_path() {
        let resolved = resolve_config_path(
            Some("/tmp/cli.yaml"),
            Some(OsString::from("/tmp/env.yaml")),
            DEFAULT_CONFIG_PATH,
        );

        assert_eq!(resolved, Some(PathBuf::from("/tmp/cli.yaml")));
    }

    #[test]
    fn falls_back_to_env_config_path() {
        let resolved = resolve_config_path(
            None,
            Some(OsString::from("/tmp/env.yaml")),
            DEFAULT_CONFIG_PATH,
        );

        assert_eq!(resolved, Some(PathBuf::from("/tmp/env.yaml")));
    }
}
