#![cfg(target_os = "linux")]

mod common;

use anyhow::{Result, anyhow};
use common::{FakeIp, write_executable};
use gruezi::gruezi::{
    addresses::{AddressAction, AddressManager},
    ha::{HaAuth, HaRuntimeConfig, HaState, HaStatus, run_with_status},
    hooks::HaHooks,
};
use std::{
    fs,
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
        peer_id: if node_id == "node-a" {
            "node-b"
        } else {
            "node-a"
        }
        .to_owned(),
        failure_policy: gruezi::config::FailurePolicy::Available,
        withdraw_timeout_ms: 5_000,
        command_timeout_ms: 3_000,
        watchdog: gruezi::config::WatchdogConfig::default(),
        authority: None,
        priority,
        preempt,
        advert_interval_ms: 40,
        dead_factor: 2,
        hold_down_ms: 40,
        jitter_ms: 0,
        auth: HaAuth::SharedKey {
            key: "shared-secret-for-tests".to_owned(),
        },
        hooks: HaHooks::default(),
        ip_command: commands.ip,
        arping_command: commands.arping,
        ndsend_command: commands.ndsend,
    }
}

fn free_udp_port() -> Result<u16> {
    use std::sync::atomic::{AtomicU16, Ordering};

    // Allocate a loopback port from a per-process window instead of asking the OS
    // for an ephemeral port and immediately closing it. That close-then-rebind gap
    // lets a parallel test grab the same number before the node binds it, causing
    // intermittent EADDRINUSE ("status channel closed"). Keying the window on the
    // PID keeps concurrently-running test binaries (whose PIDs differ by a small
    // amount) on disjoint ranges, and a per-process counter keeps parallel tests in
    // the same binary distinct; the bind check only guards the rare clash with an
    // unrelated process. Fresh sockets are still used, so packet-delivery semantics
    // are unchanged.
    static NEXT: AtomicU16 = AtomicU16::new(0);
    const WINDOW: u16 = 200;
    let pid_slot = u16::try_from(std::process::id() % u32::from(WINDOW)).unwrap_or(0);
    let base = 10_000_u16 + pid_slot * WINDOW;
    for _ in 0..WINDOW {
        let offset = NEXT.fetch_add(1, Ordering::Relaxed) % WINDOW;
        let port = base + offset;
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }

    Err(anyhow!("no free UDP port available for test node"))
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| StdDuration::from_secs(0))
        .as_nanos();
    // Exclusive directory ownership: create_dir is atomic and fails when the
    // name exists, so this test can never share state with a leftover or
    // concurrently-created directory (create_dir_all would silently reuse
    // one). Retry with the next per-process counter until the dir is ours.
    let mut attempts = 0u32;
    loop {
        let path = std::env::temp_dir().join(format!(
            "gruezi-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempts += 1;
                if attempts >= 16 {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_command_script(dir: &Path, name: &str, output_path: &Path) -> Result<String> {
    let script_path = dir.join(format!("{name}.sh"));
    let script = format!(
        "#!/usr/bin/env bash\nset -eu\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
        output_path.display()
    );
    write_executable(&script_path, &script)?;
    Ok(script_path.display().to_string())
}

fn write_ip_script(dir: &Path, name: &str, output_path: &Path, fixture: &FakeIp) -> Result<String> {
    let path = dir.join(format!("{name}.sh"));
    let script = format!(
        "#!/bin/bash\nset -eu\nprintf '%s\\n' \"$*\" >> \"{}\"\nexec \"{}\" \"$@\"\n",
        output_path.display(),
        fixture.command("ip").display()
    );
    write_executable(&path, &script)?;
    Ok(path.display().to_string())
}

async fn wait_for_log_contents(
    path: &Path,
    expected: &[&str],
    description: &str,
) -> Result<String> {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && expected.iter().all(|line| contents.contains(line))
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
    let fixture_a = FakeIp::new()?;
    let fixture_b = FakeIp::new()?;
    let ip_a = write_ip_script(&dir, "node-a-ip", &log_a, &fixture_a)?;
    let ip_b = write_ip_script(&dir, "node-b-ip", &log_b, &fixture_b)?;
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
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup state", |status| {
        status.state == HaState::Standby && status.peer_alive
    })
    .await?;

    let contents_a = wait_for_log_contents(
        &log_a,
        &["-U -I lo -c 1 10.0.0.10", "fd00::10 lo"],
        "node-a address actions",
    )
    .await?;
    // Wait for the exact actions this test asserts, not a line count: a count
    // can be satisfied by unrelated earlier lines before the dels are logged.
    let contents_b = wait_for_log_contents(
        &log_b,
        &[
            "address del 10.0.0.10/24 dev lo",
            "address del fd00::10/64 dev lo",
        ],
        "node-b address actions",
    )
    .await?;

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
async fn shutdown_removes_master_addresses() -> Result<()> {
    let dir = temp_dir("ha-addresses-shutdown")?;
    let log = dir.join("node-a.log");
    let fixture = FakeIp::new()?;
    let ip = write_ip_script(&dir, "node-a-ip", &log, &fixture)?;
    let arping = write_command_script(&dir, "node-a-arping", &log)?;
    let ndsend = write_command_script(&dir, "node-a-ndsend", &log)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = runtime_config(
        "node-a",
        port_a,
        port_b,
        110,
        true,
        CommandPaths { ip, arping, ndsend },
    );

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a standalone master state", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let startup_contents = wait_for_log_contents(
        &log,
        &[
            "address add 10.0.0.10/24 dev lo",
            "address add fd00::10/64 dev lo",
        ],
        "node-a startup address actions",
    )
    .await?;
    assert!(startup_contents.contains("address add 10.0.0.10/24 dev lo"));
    assert!(startup_contents.contains("address add fd00::10/64 dev lo"));

    stop_node(shutdown_a, task_a).await?;

    let contents = wait_for_log_contents(
        &log,
        &[
            "address del 10.0.0.10/24 dev lo",
            "address del fd00::10/64 dev lo",
        ],
        "node-a shutdown address cleanup",
    )
    .await?;
    assert!(contents.contains("address del 10.0.0.10/24 dev lo"));
    assert!(contents.contains("address del fd00::10/64 dev lo"));

    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn demotion_removes_addresses_after_preemption() -> Result<()> {
    let dir = temp_dir("ha-addresses-demote")?;
    let log_a = dir.join("node-a.log");
    let log_b = dir.join("node-b.log");
    let fixture_a = FakeIp::new()?;
    let fixture_b = FakeIp::new()?;
    let ip_a = write_ip_script(&dir, "node-a-ip", &log_a, &fixture_a)?;
    let ip_b = write_ip_script(&dir, "node-b-ip", &log_b, &fixture_b)?;
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
        status.state == HaState::Active && !status.peer_alive
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
        status.state == HaState::Active && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup after demotion", |status| {
        status.state == HaState::Standby && status.peer_alive
    })
    .await?;

    let contents_a = wait_for_log_contents(
        &log_a,
        &["-U -I lo -c 1 10.0.0.10", "fd00::10 lo"],
        "node-a address actions",
    )
    .await?;
    // Wait for the exact actions this test asserts, not a line count: a count
    // can be satisfied by unrelated earlier lines before the dels are logged.
    let contents_b = wait_for_log_contents(
        &log_b,
        &[
            "address add 10.0.0.10/24 dev lo",
            "address del 10.0.0.10/24 dev lo",
            "address del fd00::10/64 dev lo",
        ],
        "node-b address actions",
    )
    .await?;

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

fn address_manager(fixture: &FakeIp) -> AddressManager {
    AddressManager {
        ip_command: fixture.command("ip").display().to_string(),
        arping_command: fixture.command("arping").display().to_string(),
        ndsend_command: fixture.command("ndsend").display().to_string(),
        interface: "eth0".to_owned(),
        addresses: vec![
            "10.0.0.10/24".to_owned(),
            "10.0.0.11/24".to_owned(),
            "fd00::10/64".to_owned(),
        ],
    }
}

async fn read_back(fixture: &FakeIp) -> Result<String> {
    let output = timeout(
        Duration::from_secs(4),
        tokio::process::Command::new(fixture.command("ip"))
            .args(["-o", "address", "show", "dev", "eth0"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(output.status.success());
    Ok(String::from_utf8(output.stdout)?)
}

#[tokio::test]
async fn three_addresses_add_and_remove_match_read_back() -> Result<()> {
    let fixture = FakeIp::new()?;
    let dir = fixture.dir.clone();
    let manager = address_manager(&fixture);
    manager.apply(AddressAction::Add).await?;
    let table = read_back(&fixture).await?;
    println!("AFTER ADD:\n{table}");
    assert_eq!(table.lines().count(), 3);
    // Duplicate command failures are harmless only with matching read-back.
    manager.apply(AddressAction::Add).await?;
    manager.apply(AddressAction::Remove).await?;
    let table = read_back(&fixture).await?;
    println!("AFTER REMOVE:\n{table:?}");
    assert_eq!(table, "");
    manager.apply(AddressAction::Remove).await?;
    drop(fixture);
    assert!(!dir.exists());
    Ok(())
}

#[tokio::test]
async fn first_add_failure_still_attempts_every_address() -> Result<()> {
    let fixture = FakeIp::new()?;
    fs::write(fixture.dir.join("fail-add"), "1\n")?;
    let error = address_manager(&fixture)
        .apply(AddressAction::Add)
        .await
        .err()
        .ok_or_else(|| anyhow!("expected failure"))?;
    println!("FIRST ADD FAILURE: {error:#}");
    assert_eq!(fs::read_to_string(fixture.dir.join("count-add"))?, "3\n");
    assert_eq!(read_back(&fixture).await?.lines().count(), 2);
    assert!(!fs::read_to_string(fixture.dir.join("commands"))?.contains("arping"));
    Ok(())
}

#[tokio::test]
async fn stubborn_remove_reports_failure_and_removes_other_addresses() -> Result<()> {
    let fixture = FakeIp::new()?;
    let manager = address_manager(&fixture);
    manager.apply(AddressAction::Add).await?;
    fs::write(fixture.dir.join("fail-del"), "1\n")?;
    let error = manager
        .apply(AddressAction::Remove)
        .await
        .err()
        .ok_or_else(|| anyhow!("expected failure"))?;
    println!("STUBBORN REMOVE: {error:#}");
    let table = read_back(&fixture).await?;
    assert_eq!(table.lines().count(), 1);
    assert!(table.contains("10.0.0.10/24"));
    assert_eq!(fs::read_to_string(fixture.dir.join("count-del"))?, "3\n");
    Ok(())
}

#[tokio::test]
async fn file_exists_on_wrong_interface_and_prefix_does_not_confirm_presence() -> Result<()> {
    let fixture = FakeIp::new()?;
    fs::write(
        fixture.dir.join("state"),
        "other0 10.0.0.10/24\neth0 10.0.0.10/32\n",
    )?;
    let wrapper = fixture.dir.join("duplicate");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/bash\nif [[ \"$1\" == address ]]; then echo 'File exists' >&2; exit 2; fi\nexec \"{}\" \"$@\"\n",
            fixture.command("ip").display()
        ),
    )?;
    let mut manager = address_manager(&fixture);
    manager.addresses.truncate(1);
    manager.ip_command = wrapper.display().to_string();
    assert!(manager.apply(AddressAction::Add).await.is_err());
    Ok(())
}

#[tokio::test]
async fn fifo_gated_command_is_killed_after_real_timeout() -> Result<()> {
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
    let wrapper = fixture.dir.join("tracked-ip");
    let pid_path = fixture.dir.join("pid");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/bash\nif [[ \"$1\" == address ]]; then echo $$ > \"{}\"; fi\nexec \"{}\" \"$@\"\n",
            pid_path.display(),
            fixture.command("ip").display()
        ),
    )?;
    let mut manager = address_manager(&fixture);
    manager.addresses.truncate(1);
    manager.ip_command = wrapper.display().to_string();
    let start = tokio::time::Instant::now();
    let result = timeout(Duration::from_secs(6), manager.apply(AddressAction::Add)).await?;
    let error = result.err().ok_or_else(|| anyhow!("expected timeout"))?;
    assert!(error.to_string().contains("timed out after 3000 ms"));
    assert!(start.elapsed() >= Duration::from_millis(3000));
    let pid = fs::read_to_string(pid_path)?;
    timeout(Duration::from_secs(2), async {
        loop {
            if !tokio::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status()
                .await?
                .success()
            {
                return anyhow::Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    println!("FIFO TIMEOUT: {error:#}; child {} exited", pid.trim());
    assert_eq!(read_back(&fixture).await?, "");
    let dir = fixture.dir.clone();
    drop(fixture);
    assert!(!dir.exists());
    Ok(())
}
