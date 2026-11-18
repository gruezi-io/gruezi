use crate::config::{Config, DEFAULT_API_PORT, DEFAULT_HA_BIND, Mode};
use crate::gruezi::{
    addresses::WITHDRAW_TIMEOUT_MS,
    api,
    ha::{self, HaRuntimeConfig, HaStatus},
    net::bind_tcp_listener,
    signal,
};
use anyhow::{Context, Result};
use std::{future::Future, time::Duration};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

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
    run_ha_service(
        runtime,
        async { Ok(bind_tcp_listener(DEFAULT_API_PORT, None).await?.0) },
        signal::shutdown_signal(),
    )
    .await
}

async fn run_ha_service<L, S>(runtime: HaRuntimeConfig, listener: L, shutdown: S) -> Result<()>
where
    L: Future<Output = Result<TcpListener>> + Send + 'static,
    S: Future<Output = Result<()>>,
{
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

    let mut api_task = tokio::spawn(async move {
        let listener = listener.await?;
        api::run_ha_api(listener, status_tx, wait_for_shutdown(shutdown_rx)).await
    });

    let (exit_result, ha_finished, api_finished) = tokio::select! {
        signal = shutdown => (signal, false, false),
        result = &mut ha_task => {
            (handle_task_result(result, "HA runtime"), true, false)
        }
        result = &mut api_task => {
            (handle_task_result(result, "HA status API"), false, true)
        }
    };
    shutdown_tx.send_replace(true);

    // Never cancel the HA task: it owns the resource worker and drains mutations
    // through OwnershipController::shutdown's bounded withdrawal and worker join.
    // Join both peers before propagating any signal, bind, or runtime error.
    let (ha_result, api_result) = tokio::join!(
        async {
            if ha_finished {
                Ok(())
            } else {
                await_task(ha_task, "HA runtime").await
            }
        },
        async {
            if api_finished {
                Ok(())
            } else {
                stop_api_task(api_task).await
            }
        },
    );
    exit_result.and(ha_result).and(api_result)
}

async fn stop_api_task(mut task: JoinHandle<Result<()>>) -> Result<()> {
    match tokio::time::timeout(Duration::from_millis(WITHDRAW_TIMEOUT_MS), &mut task).await {
        Ok(result) => handle_task_result(result, "HA status API"),
        Err(error) => {
            task.abort();
            let _ = task.await;
            Err(error).context("HA status API shutdown timed out")
        }
    }
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

#[cfg(test)]
#[path = "../../tests/common/mod.rs"]
mod test_common;

#[cfg(test)]
mod tests {
    use super::{run_ha_service, test_common::FakeIp};
    use crate::gruezi::{
        ha::{HaAuth, HaRuntimeConfig},
        hooks::HaHooks,
    };
    use anyhow::{Context, Result, anyhow};
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };
    use tokio::{net::TcpListener, time::timeout};

    fn runtime(fixture: &FakeIp) -> HaRuntimeConfig {
        HaRuntimeConfig {
            node_id: "node-a".to_owned(),
            group_id: "cluster-ha".to_owned(),
            bind: "127.0.0.1:0".to_owned(),
            peer: "127.0.0.1:9".to_owned(),
            interface: "lo".to_owned(),
            addresses: vec!["10.0.0.10/24".to_owned()],
            protocol_version: 1,
            priority: 110,
            preempt: true,
            advert_interval_ms: 40,
            dead_factor: 2,
            hold_down_ms: 40,
            jitter_ms: 0,
            auth: HaAuth::SharedKey {
                key: "shared-secret".to_owned(),
            },
            hooks: HaHooks::default(),
            ip_command: fixture.command("ip").display().to_string(),
            arping_command: fixture.command("arping").display().to_string(),
            ndsend_command: fixture.command("ndsend").display().to_string(),
        }
    }

    async fn wait_for_contents(path: &Path, expected: &str) -> Result<()> {
        timeout(Duration::from_secs(3), async {
            loop {
                if fs::read_to_string(path).is_ok_and(|contents| contents.contains(expected)) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("waiting for fake kernel operation")?;
        Ok(())
    }

    #[tokio::test]
    async fn api_bind_failure_joins_ha_and_withdraws_vip() -> Result<()> {
        let fixture = FakeIp::new()?;
        let occupied = TcpListener::bind("127.0.0.1:0").await?;
        let addr = occupied.local_addr()?;
        let state = fixture.dir.join("state");
        let listener = async move {
            wait_for_contents(&state, "lo 10.0.0.10/24").await?;
            TcpListener::bind(addr)
                .await
                .context("injected API bind failure")
        };
        let error = timeout(
            Duration::from_secs(5),
            run_ha_service(runtime(&fixture), listener, std::future::pending()),
        )
        .await?
        .err()
        .context("expected API bind failure")?;
        let table = fs::read_to_string(fixture.dir.join("state"))?;
        println!(
            "API BIND FAILURE {addr}: {error:#}\nSIMULATED KERNEL TABLE AFTER RETURN: {table:?}"
        );
        assert!(format!("{error:#}").contains("injected API bind failure"));
        assert_eq!(table, "");
        assert!(fs::read_to_string(fixture.dir.join("commands"))?.contains("address del"));
        drop(occupied);
        Ok(())
    }

    async fn stop_after_present(state: PathBuf, signal_error: bool) -> Result<()> {
        wait_for_contents(&state, "lo 10.0.0.10/24").await?;
        if signal_error {
            Err(anyhow!("injected signal failure"))
        } else {
            Ok(())
        }
    }

    #[tokio::test]
    async fn signal_success_and_error_join_before_return() -> Result<()> {
        for signal_error in [false, true] {
            let fixture = FakeIp::new()?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let result = timeout(
                Duration::from_secs(5),
                run_ha_service(
                    runtime(&fixture),
                    async move { Ok(listener) },
                    stop_after_present(fixture.dir.join("state"), signal_error),
                ),
            )
            .await?;
            assert_eq!(result.is_err(), signal_error);
            assert_eq!(fs::read_to_string(fixture.dir.join("state"))?, "");
            assert!(TcpListener::bind(addr).await.is_ok());
        }
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_during_in_flight_add_drains_before_service_return() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let fixture = FakeIp::new()?;
        let gate = fixture.dir.join("gate-add");
        assert!(
            tokio::process::Command::new("mkfifo")
                .arg(&gate)
                .kill_on_drop(true)
                .status()
                .await?
                .success()
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let count = fixture.dir.join("count-add");
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
        let signal = async move {
            wait_for_contents(&count, "1").await?;
            let _ = signal_tx.send(());
            Ok(())
        };
        let mut service = tokio::spawn(run_ha_service(
            runtime(&fixture),
            async move { Ok(listener) },
            signal,
        ));
        timeout(Duration::from_secs(3), signal_rx).await??;
        assert!(!service.is_finished());
        // Releasing the FIFO lets the old add complete after shutdown was
        // requested. The worker must drain it and compensate before returning.
        let mut writer = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate)
            .await?;
        writer.write_all(b"release\n").await?;
        timeout(Duration::from_secs(5), &mut service).await???;
        assert_eq!(fs::read_to_string(fixture.dir.join("state"))?, "");
        println!(
            "IN-FLIGHT ADD: service pending before release; drained and withdrawn before return; kernel table empty"
        );
        Ok(())
    }
}
