mod common;

use anyhow::{Result, anyhow};
use common::{FakeIp, write_executable};
use gruezi::gruezi::{
    ha::{HaAuth, HaRuntimeConfig, HaState, HaStatus, run_with_status},
    hooks::HaHooks,
};
use std::{
    fs,
    path::{Path, PathBuf},
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
    fixture: &FakeIp,
) -> HaRuntimeConfig {
    HaRuntimeConfig {
        node_id: node_id.to_owned(),
        group_id: "cluster-ha".to_owned(),
        bind: format!("127.0.0.1:{bind_port}"),
        peer: format!("127.0.0.1:{peer_port}"),
        interface: "lo".to_owned(),
        addresses: vec!["10.0.0.10/24".to_owned()],
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
        lifecycle: None,
        priority,
        preempt,
        advert_interval_ms: 40,
        dead_factor: 2,
        // A peer counts only after a fresh session: its nonce is learned from one
        // advert and our nonce must come back in the next (~2 advert intervals).
        // The startup deadline (dead timeout + hold-down) keeps a wide margin over
        // that so the lower-priority node never promotes while still connecting.
        hold_down_ms: 200,
        jitter_ms: 0,
        auth: HaAuth::SharedKey {
            key: "shared-secret-for-tests".to_owned(),
        },
        hooks,
        ip_command: fixture.command("ip").display().to_string(),
        arping_command: fixture.command("arping").display().to_string(),
        ndsend_command: fixture.command("ndsend").display().to_string(),
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
    let mut attempts = 0_u32;
    loop {
        let path = std::env::temp_dir().join(format!(
            "gruezi-{name}-{}-{}",
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

fn write_hook_script(dir: &Path, name: &str, output_path: &Path) -> Result<String> {
    let script_path = dir.join(format!("{name}.sh"));
    let script = format!(
        "#!/usr/bin/env bash\nset -eu\nprintf '%s|%s|%s|%s|%s|%s|%s|%s\\n' \"${{GRUEZI_EVENT}}\" \"${{GRUEZI_STATE}}\" \"${{GRUEZI_PREVIOUS_STATE}}\" \"${{GRUEZI_REASON:-}}\" \"${{GRUEZI_PRIORITY}}\" \"${{GRUEZI_PEER_ID:-}}\" \"${{GRUEZI_PEER_STATE:-}}\" \"${{GRUEZI_PEER_PRIORITY:-}}\" >> \"{}\"\n",
        output_path.display()
    );
    write_executable(&script_path, &script)?;
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
    let fixture_a = FakeIp::new()?;
    let fixture_b = FakeIp::new()?;
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
        &fixture_a,
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
        &fixture_b,
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
    assert_eq!(
        fs::read_to_string(fixture_a.dir.join("state"))?,
        "lo 10.0.0.10/24\n"
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn demote_hook_runs_when_master_steps_down() -> Result<()> {
    let dir = temp_dir("ha-hooks-demote")?;
    let fixture_a = FakeIp::new()?;
    let fixture_b = FakeIp::new()?;
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
        &fixture_b,
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
        HaHooks::default(),
        &fixture_a,
    );
    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a master after preemption", |status| {
        status.state == HaState::Active && status.owned && status.peer_alive
    })
    .await?;
    wait_for_status(&mut status_b, "node-b backup after demotion", |status| {
        status.state == HaState::Standby && status.peer_alive
    })
    .await?;

    let demote_line = wait_for_hook_line(&demote_output, "demote hook output").await?;
    // The requester is still Standby when it asks the incumbent to release;
    // it cannot become Active until withdrawal confirms absence.
    assert!(
        demote_line
            .starts_with("demote|STANDBY|ACTIVE|PEER_HIGHER_PRIORITY|100|node-a|STANDBY|110")
    );
    assert_eq!(fs::read_to_string(fixture_b.dir.join("state"))?, "");
    assert_eq!(
        fs::read_to_string(fixture_a.dir.join("state"))?,
        "lo 10.0.0.10/24\n"
    );

    stop_node(shutdown_a, task_a).await?;
    stop_node(shutdown_b, task_b).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn fault_hook_runs_when_address_action_fails() -> Result<()> {
    let dir = temp_dir("ha-hooks-fault")?;
    let fixture_a = FakeIp::new()?;
    let fault_output = dir.join("fault.log");
    let promote_output = dir.join("promote.log");
    let fault_hook = write_hook_script(&dir, "fault", &fault_output)?;
    let promote_hook = write_hook_script(&dir, "promote", &promote_output)?;

    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let runtime_a = HaRuntimeConfig {
        hooks: HaHooks {
            on_promote: Some(promote_hook),
            on_fault: Some(fault_hook),
            ..HaHooks::default()
        },
        ip_command: "false".to_owned(),
        arping_command: "true".to_owned(),
        ndsend_command: "true".to_owned(),
        ..runtime_config(
            "node-a",
            port_a,
            port_b,
            110,
            true,
            HaHooks::default(),
            &fixture_a,
        )
    };

    let (mut status_a, shutdown_a, task_a) = spawn_node(runtime_a);

    wait_for_status(&mut status_a, "node-a standalone master state", |status| {
        status.state == HaState::Active && !status.peer_alive
    })
    .await?;

    let fault_contents = wait_for_hook_contents(&fault_output, 2, "fault hook output").await?;
    assert!(fault_contents.contains("fault|STANDBY|INIT|ADDRESS_ACTION_FAILED|110|||"));
    assert!(fault_contents.contains("fault|ACTIVE|STANDBY|ADDRESS_ACTION_FAILED|110|||"));
    // Under the confirmed-ownership contract, on_promote must never fire when read-back fails.
    assert!(!promote_output.exists());

    stop_node(shutdown_a, task_a).await?;
    fs::remove_dir_all(dir)?;
    Ok(())
}

/// The real runtime with `ha.lifecycle` never runs a lifecycle script, an
/// address command or a notification at the daemon identity: without the
/// packaged accounts it refuses to start at all.
#[tokio::test]
async fn lifecycle_runtime_without_packaged_identities_fails_closed() -> Result<()> {
    use gruezi::config::LifecycleConfig;

    let dir = temp_dir("ha-hooks-lifecycle-runtime")?;
    let fixture = FakeIp::new()?;
    let marker = dir.join("lifecycle.log");
    let script = dir.join("lifecycle.sh");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\necho \"$GRUEZI_EVENT $GRUEZI_INSTANCE_ID\" >> '{}'\necho db_incarnation=real-1\n",
            marker.display()
        ),
    )?;
    let promote_output = dir.join("promote.log");
    let promote_hook = write_hook_script(&dir, "promote", &promote_output)?;
    let port_a = free_udp_port()?;
    let port_b = free_udp_port()?;
    let mut runtime = runtime_config(
        "node-a",
        port_a,
        port_b,
        110,
        true,
        HaHooks {
            on_promote: Some(promote_hook),
            ..HaHooks::default()
        },
        &fixture,
    );
    runtime.lifecycle = Some(LifecycleConfig {
        activate_command: script.display().to_string(),
        deactivate_command: script.display().to_string(),
        timeout_ms: 1_000,
    });
    let (_status, shutdown, task) = spawn_node(runtime);
    // SAFETY: getpwnam reads a NUL-terminated literal and returns a pointer
    // that is only compared with null here.
    let packaged = unsafe {
        !libc::getpwnam(c"gruezi-actuator".as_ptr()).is_null()
            && !libc::getpwnam(c"gruezi-notify".as_ptr()).is_null()
    };
    if packaged {
        // A packaged host may start; startup still runs no script as the
        // daemon, which the executor's identity tests prove separately.
        stop_node(shutdown, task).await?;
        println!("lifecycle runtime started with packaged identities");
    } else {
        let result = timeout(Duration::from_secs(3), task)
            .await
            .map_err(|_| anyhow!("lifecycle runtime must refuse to start"))?
            .map_err(|error| anyhow!(error))?;
        let error = result
            .err()
            .ok_or_else(|| anyhow!("lifecycle runtime started without packaged identities"))?;
        println!("lifecycle runtime refused: {error:#}");
        assert!(format!("{error:#}").contains("lifecycle"));
        assert!(!marker.exists(), "no lifecycle script ran");
        assert!(!promote_output.exists(), "no notification ran");
        assert_eq!(fs::read_to_string(fixture.dir.join("state"))?, "");
        assert!(
            !fs::read_to_string(fixture.dir.join("commands"))
                .unwrap_or_default()
                .contains(" add "),
            "no address was added"
        );
        drop(shutdown);
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn lifecycle_executor_production_identity_is_fail_closed() -> Result<()> {
    use gruezi::{
        config::LifecycleConfig,
        gruezi::hooks::{
            ActivationMode, LifecycleContext, LifecycleExecutor, LifecycleOperation,
            LifecycleOutcome,
        },
    };

    let dir = temp_dir("ha-hooks-production-identity")?;
    let script = dir.join("identity.sh");
    write_executable(&script, "#!/bin/sh\nid -u >&2\n")?;
    let started = LifecycleExecutor::spawn(
        LifecycleConfig {
            activate_command: script.display().to_string(),
            deactivate_command: script.display().to_string(),
            timeout_ms: 1_000,
        },
        HaHooks::default(),
        "node-a".to_owned(),
        "group-a".to_owned(),
        [1; 16],
    );
    match started {
        Err(error) => {
            // Developer hosts without the packaged accounts refuse startup.
            assert!(error.to_string().contains("lifecycle"));
            println!("production lifecycle startup refused: {error}");
        }
        Ok((executor, mut events)) => {
            executor.submit(
                LifecycleOperation::Activate(ActivationMode::Cold),
                1,
                LifecycleContext {
                    authority_source: None,
                    authority_term: 0,
                    transfer_id: None,
                    peer_nonce: None,
                    deactivate_deadline: None,
                    db_incarnation: None,
                    notification: None,
                },
            )?;
            let result = timeout(Duration::from_secs(3), events.recv())
                .await?
                .ok_or_else(|| anyhow!("executor channel closed"))?;
            // A non-root developer cannot switch identities. A privileged
            // package/lab runner may execute using the real packaged account.
            assert!(matches!(
                result.outcome,
                LifecycleOutcome::Succeeded | LifecycleOutcome::SpawnError
            ));
            if result.outcome == LifecycleOutcome::Succeeded {
                // SAFETY: getuid/geteuid are argument-free identity getters.
                let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
                let child_uid = result.stderr_tail.parse::<u32>()?;
                assert_ne!(child_uid, uid);
                assert_ne!(child_uid, euid);
                assert_ne!(child_uid, 0);
            }
            println!("production identity path: {:?}", result.outcome);
            executor.shutdown().await?;
        }
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}
