use crate::config::{Config, DEFAULT_HA_BIND, Mode};
use crate::gruezi::{
    api,
    ha::{self, HaRuntimeConfig, HaStatus},
};
use anyhow::{Context, Result};
use tokio::{sync::watch, task::JoinHandle};

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
            start_ha_service(runtime).await
        }
        Mode::Kv => {
            anyhow::bail!(
                "mode 'kv' configuration is defined, but the KV runtime is not implemented yet"
            )
        }
    }
}

async fn start_ha_service(runtime: HaRuntimeConfig) -> Result<()> {
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

    let (status_tx, _status_rx) = watch::channel(HaStatus::new(
        runtime.node_id.clone(),
        runtime.group_id.clone(),
        runtime.bind.clone(),
        runtime.peer.clone(),
    ));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let ha_runtime = runtime;
    let ha_status_tx = status_tx.clone();
    let ha_shutdown = shutdown_rx.clone();
    let mut ha_task = tokio::spawn(async move {
        ha::run_with_status(
            ha_runtime,
            Some(ha_status_tx),
            wait_for_shutdown(ha_shutdown),
        )
        .await
    });

    let mut api_task =
        tokio::spawn(
            async move { api::run_ha_api(status_tx, wait_for_shutdown(shutdown_rx)).await },
        );

    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for shutdown signal")?;
            let _ = shutdown_tx.send(true);
        }
        result = &mut ha_task => {
            let _ = shutdown_tx.send(true);
            return handle_task_result(result, "HA runtime");
        }
        result = &mut api_task => {
            let _ = shutdown_tx.send(true);
            return handle_task_result(result, "HA status API");
        }
    }

    await_task(ha_task, "HA runtime").await?;
    await_task(api_task, "HA status API").await
}

async fn await_task(task: JoinHandle<Result<()>>, name: &str) -> Result<()> {
    handle_task_result(task.await, name)
}

fn handle_task_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    name: &str,
) -> Result<()> {
    result
        .with_context(|| format!("{name} task failed to join"))?
        .with_context(|| format!("{name} task returned an error"))
}

async fn wait_for_shutdown(mut shutdown_rx: watch::Receiver<bool>) {
    if *shutdown_rx.borrow() {
        return;
    }

    while shutdown_rx.changed().await.is_ok() {
        if *shutdown_rx.borrow() {
            break;
        }
    }
}
