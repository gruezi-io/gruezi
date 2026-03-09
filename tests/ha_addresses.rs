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

struct CommandPaths {
    ip: String,
    arping: String,
    ndsend: String,
}

fn runtime_config(
    node_id: &str,
    bind_port: u16,
    peer_port: u16,
    priority: u8,
    preempt: bool,
    commands: CommandPaths,
) -> HaRuntimeConfig {
    HaRuntimeConfig {
        node_id: node_id.to_owned(),
        group_id: "cluster-ha".to_owned(),
        bind: format!("127.0.0.1:{bind_port}"),
        peer: format!("127.0.0.1:{peer_port}"),
        interface: "lo".to_owned(),
        addresses: vec!["10.0.0.10/24".to_owned(), "fd00::10/64".to_owned()],
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
        ip_command: commands.ip,
        arping_command: commands.arping,
        ndsend_command: commands.ndsend,
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

fn write_command_script(dir: &Path, name: &str, output_path: &Path) -> Result<String> {
    let script_path = dir.join(format!("{name}.sh"));
    let script = format!(
        "#!/usr/bin/env bash\nset -eu\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
        output_path.display()
    );
    fs::write(&script_path, script)?;
    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    Ok(script_path.display().to_string())
}

async fn wait_for_log_lines(
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
async fn master_adds_and_backup_removes_addresses() -> Result<()> {
    let dir = temp_dir("ha-addresses-startup")?;
    let log_a = dir.join("node-a.log");
    let log_b = dir.join("node-b.log");
    let ip_a = write_command_script(&dir, "node-a-ip", &log_a)?;
    let ip_b = write_command_script(&dir, "node-b-ip", &log_b)?;
    let arping_a = write_command_script(&dir, "node-a-arping", &log_a)?;
    let arping_b = write_command_script(&dir, "node-b-arping", &log_b)?;
    let ndsend_a = write_command_script(&dir, "node-a-ndsend", &log_a)?;
    let ndsend_b = write_command_script(&dir, "node-b-ndsend", &log_b)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config(
        "node-a",
        port_a,
        port_b,
        110,
        true,
        CommandPaths {
            ip: ip_a,
            arping: arping_a,
            ndsend: ndsend_a,
        },
    );
    let runtime_b = runtime_config(
        "node-b",
        port_b,
        port_a,
        100,
        true,
        CommandPaths {
            ip: ip_b,
            arping: arping_b,
            ndsend: ndsend_b,
        },
    );

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);
    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);

    wait_for_status(&mut status_a, "node-a master state", |status| {
        status.state == HaState::Master && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup state", |status| {
        status.state == HaState::Backup && status.peer_alive
    })
    .await?;

    let contents_a = wait_for_log_lines(&log_a, 6, "node-a address actions").await?;
    let contents_b = wait_for_log_lines(&log_b, 2, "node-b address actions").await?;

    assert!(contents_a.contains("address add 10.0.0.10/24 dev lo"));
    assert!(contents_a.contains("address add fd00::10/64 dev lo"));
    assert!(contents_a.contains("-U -I lo -c 1 10.0.0.10"));
    assert!(contents_a.contains("fd00::10 lo"));
    assert!(contents_b.contains("address del 10.0.0.10/24 dev lo"));
    assert!(contents_b.contains("address del fd00::10/64 dev lo"));

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn demotion_removes_addresses_after_preemption() -> Result<()> {
    let dir = temp_dir("ha-addresses-demote")?;
    let log_a = dir.join("node-a.log");
    let log_b = dir.join("node-b.log");
    let ip_a = write_command_script(&dir, "node-a-ip", &log_a)?;
    let ip_b = write_command_script(&dir, "node-b-ip", &log_b)?;
    let arping_a = write_command_script(&dir, "node-a-arping", &log_a)?;
    let arping_b = write_command_script(&dir, "node-b-arping", &log_b)?;
    let ndsend_a = write_command_script(&dir, "node-a-ndsend", &log_a)?;
    let ndsend_b = write_command_script(&dir, "node-b-ndsend", &log_b)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_b = runtime_config(
        "node-b",
        port_b,
        port_a,
        100,
        true,
        CommandPaths {
            ip: ip_b,
            arping: arping_b,
            ndsend: ndsend_b,
        },
    );

    let (mut status_b, shutdown_b, task_b) = spawn_node(runtime_b);
    wait_for_status(&mut status_b, "node-b standalone master state", |status| {
        status.state == HaState::Master && !status.peer_alive
    })
    .await?;

    let runtime_a = runtime_config(
        "node-a",
        port_a,
        port_b,
        110,
        true,
        CommandPaths {
            ip: ip_a,
            arping: arping_a,
            ndsend: ndsend_a,
        },
    );
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a master after preemption", |status| {
        status.state == HaState::Master && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup after demotion", |status| {
        status.state == HaState::Backup && status.peer_alive
    })
    .await?;

    let contents_a = wait_for_log_lines(&log_a, 6, "node-a address actions").await?;
    let contents_b = wait_for_log_lines(&log_b, 6, "node-b address actions").await?;

    assert!(contents_a.contains("address add 10.0.0.10/24 dev lo"));
    assert!(contents_a.contains("address add fd00::10/64 dev lo"));
    assert!(contents_a.contains("-U -I lo -c 1 10.0.0.10"));
    assert!(contents_a.contains("fd00::10 lo"));
    assert!(contents_b.contains("address add 10.0.0.10/24 dev lo"));
    assert!(contents_b.contains("address del 10.0.0.10/24 dev lo"));
    assert!(contents_b.contains("address del fd00::10/64 dev lo"));

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}
