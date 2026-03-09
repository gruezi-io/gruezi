use crate::config::{Config, DEFAULT_HA_BIND, Mode};
use crate::gruezi::ha::{self, HaRuntimeConfig};
use anyhow::Result;

/// Start the gruezi service with the given configuration
///
/// # Errors
///
/// Returns an error if the service fails to start
pub fn start_service(bind: &str, peers: Option<&str>, node_id: Option<&str>) -> Result<()> {
    // TODO: Implement service startup logic
    // - Initialize HA mode over UDP at L4 when using the direct CLI path
    // - Start the HA listener on the configured bind address
    // - Join the peer if specified
    // - Evolve this path into the YAML-backed HA runtime

    tracing::info!("Starting gruezi service on {bind}");

    if let Some(p) = peers {
        tracing::info!("Connecting to peers: {p}");
    }

    if let Some(id) = node_id {
        tracing::info!("Node ID: {id}");
    }

    if bind == DEFAULT_HA_BIND {
        tracing::info!("using default HA bind address");
    }

    Ok(())
}

/// Start the gruezi service from a validated YAML configuration.
///
/// # Errors
///
/// Returns an error if the selected mode is not yet implemented.
pub async fn start_service_with_config(config: &Config) -> Result<()> {
    match config.mode {
        Mode::Ha => {
            let runtime = HaRuntimeConfig::try_from(config)?;

            tracing::info!(
                bind = %runtime.bind,
                interface = %runtime.interface,
                peer = %runtime.peer,
                group_id = %runtime.group_id,
                protocol_version = runtime.protocol_version,
                advert_interval_ms = runtime.advert_interval_ms,
                dead_factor = runtime.dead_factor,
                address_count = runtime.addresses.len(),
                "starting gruezi in HA mode"
            );

            tracing::info!(
                node_id = %runtime.node_id,
                dead_timeout_ms = runtime.dead_timeout().as_millis(),
                hold_down_ms = runtime.hold_down().as_millis(),
                "loaded HA runtime configuration"
            );

            ha::run(runtime).await
        }
        Mode::Kv => {
            anyhow::bail!(
                "mode 'kv' configuration is defined, but the KV runtime is not implemented yet"
            )
        }
    }
}
