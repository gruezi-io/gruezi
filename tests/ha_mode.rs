use anyhow::{Result, anyhow};
use gruezi::gruezi::{
    api,
    ha::{HaAuth, HaDecisionReason, HaRuntimeConfig, HaState, HaStatus, run_with_status},
    hooks::HaHooks,
    status::fetch_status,
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

struct NodeWithApi {
    status_rx: watch::Receiver<HaStatus>,
    shutdown_tx: oneshot::Sender<()>,
    runtime_task: JoinHandle<Result<()>>,
    api_task: JoinHandle<Result<()>>,
}

fn spawn_node_with_api(runtime: HaRuntimeConfig) -> NodeWithApi {
    let (status_tx, status_rx) = watch::channel(HaStatus::new(
        runtime.node_id.clone(),
        runtime.group_id.clone(),
        runtime.bind.clone(),
        runtime.peer.clone(),
    ));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(shutdown_rx)));

    let runtime_task = {
        let status_tx = status_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            run_with_status(runtime, Some(status_tx), async move {
                let mut rx = shutdown_rx.lock().await;
                if let Some(shutdown_rx) = rx.take() {
                    let _ = shutdown_rx.await;
                }
            })
            .await
        })
    };

    let api_task =
        tokio::spawn(async move { api::run_ha_api(status_tx, std::future::pending()).await });

    NodeWithApi {
        status_rx,
        shutdown_tx,
        runtime_task,
        api_task,
    }
}

async fn wait_for_status<F>(
    status_rx: &mut watch::Receiver<HaStatus>,
    description: &str,
    predicate: F,
) -> Result<HaStatus>
where
    F: Fn(&HaStatus) -> bool,
{
    timeout(Duration::from_secs(5), async {
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

async fn stop_node_with_api(
    shutdown_tx: oneshot::Sender<()>,
    runtime_task: JoinHandle<Result<()>>,
    api_task: JoinHandle<Result<()>>,
) -> Result<()> {
    let _ = shutdown_tx.send(());
    runtime_task.await.map_err(|error| anyhow!(error))??;
    api_task.abort();
    let _ = api_task.await;
    Ok(())
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
            status.state == HaState::Active
                && status.peer_alive
                && status.peer_state == Some(HaState::Standby)
                && status.packets_received > 0
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b backup with observed master peer",
        |status| {
            status.state == HaState::Standby
                && status.peer_alive
                && status.peer_state == Some(HaState::Active)
                && status.packets_received > 0
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert!(matches!(
        snapshot_a.decision_reason,
        HaDecisionReason::LocalHigherPriority | HaDecisionReason::AlreadyActive
    ));
    assert_eq!(
        snapshot_b.decision_reason,
        HaDecisionReason::PeerHigherPriority
    );
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::LocalHigherPriority)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::StartupHold)
    );

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
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b initial backup state", |status| {
        status.state == HaState::Standby
            && status.peer_alive
            && status.peer_state == Some(HaState::Active)
    })
    .await?;

    stop_node(shutdown_a, task_a).await?;

    let snapshot_b = wait_for_status(&mut status_b, "node-b takeover after peer loss", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert_eq!(snapshot_b.decision_reason, HaDecisionReason::PeerTimeout);
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::PeerTimeout)
    );

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
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config("node-a", port_a, port_b, 110, false);
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a backup when preemption is disabled",
        |status| {
            status.state == HaState::Standby
                && status.peer_alive
                && status.peer_state == Some(HaState::Active)
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(status_b.borrow().state, HaState::Active);
    assert_eq!(
        snapshot_a.decision_reason,
        HaDecisionReason::PeerActiveNoPreempt
    );

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
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(&mut status_a, "node-a preempting as master", |status| {
        status.state == HaState::Active
            && status.peer_alive
            && status.peer_state == Some(HaState::Standby)
    })
    .await?;
    let snapshot_b = wait_for_status(&mut status_b, "node-b demoting to backup", |status| {
        status.state == HaState::Standby
            && status.peer_alive
            && status.peer_state == Some(HaState::Active)
    })
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::PreemptHigherPriority)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::PeerHigherPriority)
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn returning_higher_priority_node_reclaims_vip_when_preempt_enabled() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a.clone());
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    wait_for_status(&mut status_a, "node-a initial master state", |status| {
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b initial backup state", |status| {
        status.state == HaState::Standby && status.peer_state == Some(HaState::Active)
    })
    .await?;

    stop_node(shutdown_a, task_a).await?;

    wait_for_status(&mut status_b, "node-b takeover after peer loss", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let (mut status_a_returned, shutdown_a_returned, task_a_returned) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(
        &mut status_a_returned,
        "node-a reclaiming master role after returning",
        |status| {
            status.state == HaState::Active
                && status.peer_alive
                && status.peer_state == Some(HaState::Standby)
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b demoting after higher-priority peer returns",
        |status| {
            status.state == HaState::Standby
                && status.peer_alive
                && status.peer_state == Some(HaState::Active)
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::PreemptHigherPriority)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::PeerHigherPriority)
    );

    stop_node(shutdown_a_returned, task_a_returned).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn returning_higher_priority_node_stays_backup_when_preempt_disabled() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, false);
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a.clone());
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    wait_for_status(&mut status_a, "node-a initial master state", |status| {
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b initial backup state", |status| {
        status.state == HaState::Standby && status.peer_state == Some(HaState::Active)
    })
    .await?;

    stop_node(shutdown_a, task_a).await?;

    wait_for_status(&mut status_b, "node-b takeover after peer loss", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let (mut status_a_returned, shutdown_a_returned, task_a_returned) = spawn_node(runtime_a);

    let snapshot_a = wait_for_status(
        &mut status_a_returned,
        "node-a staying backup after returning with preemption disabled",
        |status| {
            status.state == HaState::Standby
                && status.peer_alive
                && status.peer_state == Some(HaState::Active)
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b remaining master after higher-priority peer returns",
        |status| {
            status.state == HaState::Active
                && status.peer_alive
                && status.peer_state == Some(HaState::Standby)
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert_eq!(
        snapshot_a.decision_reason,
        HaDecisionReason::PeerActiveNoPreempt
    );
    assert_eq!(snapshot_b.decision_reason, HaDecisionReason::AlreadyActive);

    stop_node(shutdown_a_returned, task_a_returned).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn equal_priority_uses_node_id_tiebreak() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 100, true);
    let runtime_b = runtime_config("node-b", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a standby on equal-priority tiebreak",
        |status| {
            status.state == HaState::Standby
                && status.peer_alive
                && status.peer_state == Some(HaState::Active)
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b active on equal-priority tiebreak",
        |status| {
            status.state == HaState::Active
                && status.peer_alive
                && status.peer_state == Some(HaState::Standby)
        },
    )
    .await?;

    assert_eq!(snapshot_a.peer_node_id.as_deref(), Some("node-b"));
    assert_eq!(snapshot_b.peer_node_id.as_deref(), Some("node-a"));
    assert_eq!(
        snapshot_a.decision_reason,
        HaDecisionReason::PeerNodeIdTiebreak
    );
    assert_eq!(snapshot_b.decision_reason, HaDecisionReason::AlreadyActive);
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::StartupHold)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::LocalNodeIdTiebreak)
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn auth_mismatch_keeps_peers_isolated() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let mut runtime_b = runtime_config("node-b", port_b, port_a, 100, true);
    runtime_b.auth = HaAuth::SharedKey {
        key: "wrong-secret".to_owned(),
    };

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a isolated by auth mismatch",
        |status| status.state == HaState::Active && !status.peer_alive && status.auth_failures > 0,
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b isolated by auth mismatch",
        |status| status.state == HaState::Active && !status.peer_alive && status.auth_failures > 0,
    )
    .await?;

    assert!(snapshot_a.invalid_packets > 0);
    assert!(snapshot_b.invalid_packets > 0);
    assert!(matches!(
        snapshot_a.decision_reason,
        HaDecisionReason::StartupDeadlineExpired | HaDecisionReason::AlreadyActive
    ));
    assert!(matches!(
        snapshot_b.decision_reason,
        HaDecisionReason::StartupDeadlineExpired | HaDecisionReason::AlreadyActive
    ));
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn group_id_mismatch_keeps_peers_isolated() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let mut runtime_b = runtime_config("node-b", port_b, port_a, 100, true);
    runtime_b.group_id = "other-ha-group".to_owned();

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a isolated by group mismatch",
        |status| {
            status.state == HaState::Active && !status.peer_alive && status.invalid_packets > 0
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "node-b isolated by group mismatch",
        |status| {
            status.state == HaState::Active && !status.peer_alive && status.invalid_packets > 0
        },
    )
    .await?;

    assert_eq!(snapshot_a.auth_failures, 0);
    assert_eq!(snapshot_b.auth_failures, 0);
    assert!(matches!(
        snapshot_a.decision_reason,
        HaDecisionReason::StartupDeadlineExpired | HaDecisionReason::AlreadyActive
    ));
    assert!(matches!(
        snapshot_b.decision_reason,
        HaDecisionReason::StartupDeadlineExpired | HaDecisionReason::AlreadyActive
    ));
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn duplicate_node_id_keeps_peers_isolated() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);
    let runtime_b = runtime_config("node-a", port_b, port_a, 100, true);

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    let snapshot_a = wait_for_status(
        &mut status_a,
        "node-a isolated by duplicate node id",
        |status| {
            status.state == HaState::Active
                && !status.peer_alive
                && status.duplicate_node_id_packets > 0
        },
    )
    .await?;
    let snapshot_b = wait_for_status(
        &mut status_b,
        "second node isolated by duplicate node id",
        |status| {
            status.state == HaState::Active
                && !status.peer_alive
                && status.duplicate_node_id_packets > 0
        },
    )
    .await?;

    assert!(snapshot_a.invalid_packets > 0);
    assert!(snapshot_b.invalid_packets > 0);
    assert_eq!(snapshot_a.auth_failures, 0);
    assert_eq!(snapshot_b.auth_failures, 0);
    assert_eq!(
        snapshot_a.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );
    assert_eq!(
        snapshot_b.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    Ok(())
}

#[tokio::test]
async fn status_api_reflects_live_ha_state() -> Result<()> {
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true);

    let NodeWithApi {
        mut status_rx,
        shutdown_tx,
        runtime_task,
        api_task,
    } = spawn_node_with_api(runtime_a);

    wait_for_status(&mut status_rx, "node-a active with api", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let response = timeout(Duration::from_secs(3), async {
        loop {
            match fetch_status(Some("127.0.0.1:9376")).await {
                Ok(response) => return Ok::<_, anyhow::Error>(response),
                Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for HA API status response"))??;

    let ha = response
        .ha
        .ok_or_else(|| anyhow!("HA API response missing ha status"))?;
    assert_eq!(response.mode, "ha");
    assert_eq!(ha.node_id, "node-a");
    assert_eq!(ha.state, HaState::Active);
    assert!(!ha.peer_alive);
    assert!(matches!(
        ha.decision_reason,
        HaDecisionReason::StartupDeadlineExpired | HaDecisionReason::AlreadyActive
    ));
    assert_eq!(
        ha.last_transition_reason,
        Some(HaDecisionReason::StartupDeadlineExpired)
    );
    assert!(ha.last_transition_ms_ago.is_some());

    stop_node_with_api(shutdown_tx, runtime_task, api_task).await?;
    Ok(())
}
