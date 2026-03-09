use anyhow::{Result, anyhow};
use gruezi::gruezi::{
    ha::{HaAuth, HaRuntimeConfig, HaState, HaStatus, run_with_status},
    hooks::HaHooks,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Duration, timeout},
};

fn runtime_config(
    node_id: &str,
    bind_port: u16,
    peer_port: u16,
    priority: u8,
    preempt: bool,
) -> HaRuntimeConfig {
    HaRuntimeConfig {
        node_id: node_id.to_owned(),
        group_id: "cluster-ha".to_owned(),
        bind: format!("127.0.0.1:{bind_port}"),
        peer: format!("127.0.0.1:{peer_port}"),
        interface: "lo".to_owned(),
        addresses: vec!["10.0.0.10/24".to_owned()],
        protocol_version: 1,
        priority,
        preempt,
        advert_interval_ms: 40,
        dead_factor: 2,
        hold_down_ms: 40,
        jitter_ms: 0,
        auth: HaAuth::SharedKey {
            key: "shared-secret".to_owned(),
        },
        hooks: HaHooks::default(),
        ip_command: "true".to_owned(),
        arping_command: "true".to_owned(),
        ndsend_command: "true".to_owned(),
    }
}

fn free_udp_port() -> Result<u16> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(socket.local_addr()?.port())
}

fn spawn_node(
    runtime: HaRuntimeConfig,
) -> (
    watch::Receiver<HaStatus>,
    oneshot::Sender<()>,
    JoinHandle<Result<()>>,
) {
    let (status_tx, status_rx) = watch::channel(HaStatus::new(
        runtime.node_id.clone(),
        runtime.group_id.clone(),
        runtime.bind.clone(),
        runtime.peer.clone(),
    ));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        run_with_status(runtime, Some(status_tx), async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    (status_rx, shutdown_tx, task)
}

async fn wait_for_status<F>(
    status_rx: &mut watch::Receiver<HaStatus>,
    description: &str,
    predicate: F,
) -> Result<HaStatus>
where
    F: Fn(&HaStatus) -> bool,
{
    timeout(Duration::from_secs(3), async {
        loop {
            let current = status_rx.borrow().clone();
            if predicate(&current) {
                return Ok(current);
            }

            status_rx
                .changed()
                .await
                .map_err(|_| anyhow!("status channel closed"))?;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for {description}"))?
}

async fn stop_node(shutdown_tx: oneshot::Sender<()>, task: JoinHandle<Result<()>>) -> Result<()> {
    let _ = shutdown_tx.send(());
    task.await.map_err(|error| anyhow!(error))?
}

#[tokio::test]
async fn higher_priority_node_wins_over_udp() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a master with observed backup peer",
        |status| {
            status.state == HaState::Master
                && status.peer_alive
                && status.peer_state == Some(HaState::Backup)
                && status.packets_received > 0
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b backup with observed master peer",
        |status| {
            status.state == HaState::Backup
                && status.peer_alive
                && status.peer_state == Some(HaState::Master)
                && status.packets_received > 0
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn backup_promotes_after_master_stops() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    wait_for_status(&mut status_a, "node-a initial master state", |status| {
        status.state == HaState::Master && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b initial backup state", |status| {
        status.state == HaState::Backup
            && status.peer_alive
            && status.peer_state == Some(HaState::Master)
    })
    .await?;

    stop_node(shutdown_a, task_a).await?;

    let snapshot_b = wait_for_status(&mut status_b, "node-b takeover after peer loss", |status| {
        status.state == HaState::Master && !status.peer_alive
    })
    .await?;
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));

    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn higher_priority_node_does_not_preempt_when_disabled() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);
    wait_for_status(&mut status_b, "node-b standalone master state", |status| {
        status.state == HaState::Master && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config("node-a", port_a, port_b, 110, false);
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a backup when preemption is disabled",
        |status| {
            status.state == HaState::Backup
                && status.peer_alive
                && status.peer_state == Some(HaState::Master)
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(status_b.borrow().state, HaState::Master);

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn higher_priority_node_preempts_when_enabled() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);
    wait_for_status(&mut status_b, "node-b standalone master state", |status| {
        status.state == HaState::Master && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(&mut status_a, "node-a preempting as master", |status| {
        status.state == HaState::Master
            && status.peer_alive
            && status.peer_state == Some(HaState::Backup)
    })
    .await?;
    let snapshot_b = wait_for_status(&mut status_b, "node-b demoting to backup", |status| {
        status.state == HaState::Backup
            && status.peer_alive
            && status.peer_state == Some(HaState::Master)
    })
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}
