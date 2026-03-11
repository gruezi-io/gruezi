use anyhow::{Result, anyhow};
use gruezi::gruezi::{
    ha::{HaAuth, HaRuntimeConfig, HaState, HaStatus, run_with_status},
    hooks::HaHooks,
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Duration, sleep, timeout},
};

fn runtime_config(
    node_id: &str,
    bind_port: u16,
    peer_port: u16,
    priority: u8,
    preempt: bool,
    hooks: HaHooks,
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
        hooks,
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

fn temp_dir(name: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| StdDuration::from_secs(0))
        .as_nanos();
    let path = std::env::temp_dir().join(format!("gruezi-{name}-{nonce}"));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn write_hook_script(dir: &Path, name: &str, output_path: &Path) -> Result<String> {
    let script_path = dir.join(format!("{name}.sh"));
    let script = format!(
        "#!/usr/bin/env bash\nset -eu\nprintf '%s|%s|%s|%s|%s|%s|%s|%s\\n' \"${{GRUEZI_EVENT}}\" \"${{GRUEZI_STATE}}\" \"${{GRUEZI_PREVIOUS_STATE}}\" \"${{GRUEZI_REASON:-}}\" \"${{GRUEZI_PRIORITY}}\" \"${{GRUEZI_PEER_ID:-}}\" \"${{GRUEZI_PEER_STATE:-}}\" \"${{GRUEZI_PEER_PRIORITY:-}}\" >> \"{}\"\n",
        output_path.display()
    );
    fs::write(&script_path, script)?;
    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    Ok(script_path.display().to_string())
}

async fn wait_for_hook_line(path: &Path, description: &str) -> Result<String> {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && let Some(line) = contents.lines().next()
            {
                return Ok(line.to_owned());
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for {description}"))?
}

async fn wait_for_hook_contents(
    path: &Path,
    expected_lines: usize,
    description: &str,
) -> Result<String> {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && contents.lines().count() >= expected_lines
            {
                return Ok(contents);
            }

            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for {description}"))?
}

#[tokio::test]
async fn promote_and_backup_hooks_record_transition_context() -> Result<()> {
    let dir = temp_dir("ha-hooks-promote")?;
    let promote_output = dir.join("promote.log");
    let backup_output = dir.join("backup.log");
    let promote_hook = write_hook_script(&dir, "promote", &promote_output)?;
    let backup_hook = write_hook_script(&dir, "backup", &backup_output)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config(
        "node-a",
        port_a,
        port_b,
        110,
        true,
        HaHooks {
            on_promote: Some(promote_hook),
            ..HaHooks::default()
        },
    );
    let runtime_b = runtime_config(
        "node-b",
        port_b,
        port_a,
        100,
        true,
        HaHooks {
            on_backup: Some(backup_hook),
            ..HaHooks::default()
        },
    );

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    wait_for_status(&mut status_a, "node-a master state", |status| {
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup state", |status| {
        status.state == HaState::Standby
    })
    .await?;

    let promote_line = wait_for_hook_line(&promote_output, "promote hook output").await?;
    let backup_line = wait_for_hook_line(&backup_output, "backup hook output").await?;

    assert!(
        promote_line
            .starts_with("promote|ACTIVE|STANDBY|LOCAL_HIGHER_PRIORITY|110|node-b|STANDBY|100")
    );
    assert!(backup_line.starts_with("backup|STANDBY|INIT|STARTUP_HOLD|100|||"));

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn demote_hook_runs_when_master_steps_down() -> Result<()> {
    let dir = temp_dir("ha-hooks-demote")?;
    let demote_output = dir.join("demote.log");
    let demote_hook = write_hook_script(&dir, "demote", &demote_output)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_b = runtime_config(
        "node-b",
        port_b,
        port_a,
        100,
        true,
        HaHooks {
            on_demote: Some(demote_hook),
            ..HaHooks::default()
        },
    );

    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);
    wait_for_status(&mut status_b, "node-b standalone master state", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config("node-a", port_a, port_b, 110, true, HaHooks::default());
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a master after preemption", |status| {
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup after demotion", |status| {
        status.state == HaState::Standby && status.peer_alive
    })
    .await?;

    let demote_line = wait_for_hook_line(&demote_output, "demote hook output").await?;
    assert!(
        demote_line.starts_with("demote|STANDBY|ACTIVE|PEER_HIGHER_PRIORITY|100|node-a|ACTIVE|110")
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn fault_hook_runs_when_address_action_fails() -> Result<()> {
    let dir = temp_dir("ha-hooks-fault")?;
    let fault_output = dir.join("fault.log");
    let fault_hook = write_hook_script(&dir, "fault", &fault_output)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = HaRuntimeConfig {
        hooks: HaHooks {
            on_fault: Some(fault_hook),
            ..HaHooks::default()
        },
        ip_command: "false".to_owned(),
        arping_command: "true".to_owned(),
        ndsend_command: "true".to_owned(),
        ..runtime_config("node-a", port_a, port_b, 110, true, HaHooks::default())
    };

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a standalone master state", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let fault_contents = wait_for_hook_contents(&fault_output, 2, "fault hook output").await?;
    assert!(fault_contents.contains("fault|STANDBY|INIT|ADDRESS_ACTION_FAILED|110|||"));
    assert!(fault_contents.contains("fault|ACTIVE|STANDBY|ADDRESS_ACTION_FAILED|110|||"));

    stop_node(shutdown_a, task_a).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}
