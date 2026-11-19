use super::*;
use anyhow::{Context, Result};
use std::{fmt::Write, fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc::UnboundedReceiver,
    time::{Duration, timeout},
};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!("{error}"))?;
        let mut name = String::with_capacity(32);
        for byte in nonce {
            write!(name, "{byte:02x}")?;
        }
        let dir = std::env::temp_dir().join(format!("gruezi-executor-{name}"));
        fs::create_dir(&dir)?;
        Ok(Self { dir })
    }

    fn script(&self, name: &str, body: &str) -> Result<String> {
        let path = self.dir.join(name);
        fs::write(&path, format!("#!/bin/bash\nset -eu\n{body}\n"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path.display().to_string())
    }

    fn executor(
        &self,
        activate: &str,
        deactivate: &str,
        timeout_ms: u64,
        hooks: HaHooks,
    ) -> Result<(LifecycleExecutor, UnboundedReceiver<LifecycleEvent>)> {
        anyhow::ensure!(self.dir.is_dir(), "fixture directory missing");
        Ok(LifecycleExecutor::spawn_for_unit_tests(
            crate::config::LifecycleConfig {
                activate_command: activate.to_owned(),
                deactivate_command: deactivate.to_owned(),
                timeout_ms,
            },
            hooks,
            "node-a".to_owned(),
            "group-a".to_owned(),
            [0x11; 16],
        ))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => println!("cleanup removed {}", self.dir.display()),
            Err(error) => eprintln!("fixture cleanup failed: {error}"),
        }
    }
}

fn context() -> LifecycleContext {
    LifecycleContext {
        authority_source: Some(AuthoritySource::Peer),
        authority_term: 42,
        transfer_id: None,
        peer_nonce: None,
        deactivate_deadline: None,
        db_incarnation: Some("db-123".to_owned()),
        notification: None,
    }
}

async fn event(events: &mut UnboundedReceiver<LifecycleEvent>) -> Result<LifecycleEvent> {
    let event = timeout(Duration::from_secs(5), events.recv())
        .await?
        .context("executor event channel closed")?;
    println!(
        "{:?}({:?}) generation={}",
        event.outcome, event.kind, event.generation
    );
    Ok(event)
}

async fn gate() -> Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let script = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port}\nprintf 'READY %s\\n' \"$$\" >&3\nread -r -t 10 token <&3\nexec 3>&-"
    );
    Ok((listener, script))
}

async fn started(listener: &TcpListener) -> Result<BufReader<TcpStream>> {
    timeout(Duration::from_secs(3), async {
        let (stream, _) = listener.accept().await?;
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        anyhow::ensure!(line.starts_with("READY "), "missing child gate: {line}");
        Ok(stream)
    })
    .await?
}

async fn gone(pid: &str) -> Result<()> {
    timeout(Duration::from_secs(3), async {
        while PathBuf::from(format!("/proc/{}", pid.trim())).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    println!("no surviving descendant PID {}", pid.trim());
    Ok(())
}

#[tokio::test]
async fn executor_passes_full_environment() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture.dir.join("env");
    let script = fixture.script(
        "environment",
        &format!(
            "/usr/bin/env > '{}'\nprintf 'db_incarnation=db-123\\n'",
            output.display()
        ),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    let mut job_context = context();
    job_context.transfer_id = Some(9);
    job_context.peer_nonce = Some([0x22; 16]);
    executor.submit(LifecycleOperation::Verify, 7, job_context)?;
    let result = event(&mut events).await?;
    assert_eq!(result.outcome, LifecycleOutcome::Succeeded);
    assert_eq!(result.db_incarnation.as_deref(), Some("db-123"));
    let env = fs::read_to_string(output)?;
    println!("observed real child environment:\n{env}");
    for pair in [
        "GRUEZI_EVENT=verify",
        "GRUEZI_GENERATION=7",
        "GRUEZI_AUTHORITY_SOURCE=peer",
        "GRUEZI_AUTHORITY_TERM=42",
        "GRUEZI_TIMEOUT_MS=1000",
        "GRUEZI_DB_INCARNATION=db-123",
        "GRUEZI_NODE_ID=node-a",
        "GRUEZI_GROUP_ID=group-a",
        "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
        "HOME=/nonexistent",
        "GRUEZI_TRANSFER_ID=9",
        "GRUEZI_PEER_NONCE=22222222222222222222222222222222",
        "LANG=C",
        "LC_ALL=C",
    ] {
        assert!(env.lines().any(|line| line == pair), "missing {pair}");
    }
    let instance = env
        .lines()
        .find_map(|line| line.strip_prefix("GRUEZI_INSTANCE_ID="))
        .context("missing instance ID")?;
    assert_eq!(instance.len(), 32);
    assert_eq!(instance, "11111111111111111111111111111111");
    assert!(instance.bytes().all(|byte| byte.is_ascii_hexdigit()));
    for forbidden in ["OTEL_", "MYSQL_PWD", "GRUEZI_ADAPTER_TEST", "GRUEZI_CONFIG"] {
        assert!(!env.contains(forbidden));
    }
    let actual_override = env
        .lines()
        .find_map(|line| line.strip_prefix("GRUEZI_MARIADB_ADAPTER_CONF="));
    let expected_override = std::env::var("GRUEZI_MARIADB_ADAPTER_CONF").ok();
    assert_eq!(actual_override, expected_override.as_deref());
    assert!(result.started.is_some());
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_passes_operation_mode_env() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture.dir.join("mode-env");
    let script = fixture.script(
        "mode-environment",
        &format!("/usr/bin/env > '{}'", output.display()),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    for (operation, expected, forbidden) in [
        (
            LifecycleOperation::Activate(ActivationMode::Handoff),
            "GRUEZI_ACTIVATION_MODE=handoff",
            "GRUEZI_DEACTIVATE_MODE=",
        ),
        (
            LifecycleOperation::Activate(ActivationMode::Cold),
            "GRUEZI_ACTIVATION_MODE=cold",
            "GRUEZI_DEACTIVATE_MODE=",
        ),
        (
            LifecycleOperation::Deactivate(DeactivationMode::Baseline),
            "GRUEZI_DEACTIVATE_MODE=baseline",
            "GRUEZI_ACTIVATION_MODE=",
        ),
        (
            LifecycleOperation::Deactivate(DeactivationMode::HandoffRelease),
            "GRUEZI_DEACTIVATE_MODE=handoff_release",
            "GRUEZI_ACTIVATION_MODE=",
        ),
        (
            LifecycleOperation::Deactivate(DeactivationMode::Revoke),
            "GRUEZI_DEACTIVATE_MODE=revoke",
            "GRUEZI_ACTIVATION_MODE=",
        ),
    ] {
        executor.submit(operation, 1, context())?;
        assert_eq!(
            event(&mut events).await?.outcome,
            LifecycleOutcome::Succeeded
        );
        let env = fs::read_to_string(&output)?;
        println!("observed {operation:?} child environment:\n{env}");
        assert!(
            env.lines().any(|line| line == expected),
            "missing {expected}"
        );
        assert!(!env.lines().any(|line| line.starts_with(forbidden)));
    }
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_omits_mode_env_for_verify() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture.dir.join("verify-env");
    let script = fixture.script(
        "verify-environment",
        &format!(
            "/usr/bin/env > '{}'\nprintf 'db_incarnation=db-123\\n'",
            output.display()
        ),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    // A read-only check of the active generation must not inherit its activation mode.
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Handoff),
        1,
        context(),
    )?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.submit(LifecycleOperation::Verify, 1, context())?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    let env = fs::read_to_string(output)?;
    println!("observed verify child environment:\n{env}");
    assert!(env.lines().any(|line| line == "GRUEZI_EVENT=verify"));
    for key in ["GRUEZI_ACTIVATION_MODE=", "GRUEZI_DEACTIVATE_MODE="] {
        assert!(!env.lines().any(|line| line.starts_with(key)));
        println!("verify child: {key}<absent>");
    }
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_bounds_output_and_strips_control_characters() -> Result<()> {
    let fixture = Fixture::new()?;
    let script = fixture.script("output", "test \"$GRUEZI_TRANSFER_ID\" = 0\ntest -z \"$GRUEZI_PEER_NONCE\"\nhead -c 200000 /dev/zero\nhead -c 200000 /dev/zero >&2\nprintf '\\001\\033bad\\177tail\\n' >&2")?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let result = event(&mut events).await?;
    assert_eq!(result.outcome, LifecycleOutcome::Succeeded);
    assert!(result.stderr_tail.len() <= 1_024);
    assert!(result.stderr_tail.ends_with("badtail"));
    assert!(!result.stderr_tail.chars().any(char::is_control));
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_timeout_kills_grandchild() -> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.dir.join("pid");
    let (listener, gate_script) = gate().await?;
    let script = fixture.script("timeout", &format!(
        "sleep 300 &\nchild=$!\nprintf '%s' \"$child\" > '{}'\ntrap 'wait \"$child\" || true; exit 0' TERM\n{gate_script}\nwait \"$child\"",
        pid_file.display()
    ))?;
    let (executor, mut events) = fixture.executor(&script, &script, 500, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let _stream = started(&listener).await?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::TimedOut
    );
    gone(&fs::read_to_string(pid_file)?).await?;
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_deactivate_cancels_running_activate_before_starting() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let activate = fixture.script("activate", &gate_script)?;
    let deactivate = fixture.script("deactivate", "exit 0")?;
    let (executor, mut events) =
        fixture.executor(&activate, &deactivate, 3_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let _stream = started(&listener).await?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        2,
        context(),
    )?;
    let cancelled = event(&mut events).await?;
    assert_eq!(
        (cancelled.kind, cancelled.outcome),
        (LifecycleKind::Activate, LifecycleOutcome::Cancelled)
    );
    let passive = event(&mut events).await?;
    assert_eq!(
        (passive.kind, passive.outcome),
        (LifecycleKind::Deactivate, LifecycleOutcome::Succeeded)
    );
    assert!(
        passive.started.context("deactivate start")?
            >= cancelled.started.context("activate start")? + cancelled.elapsed
    );
    println!("Cancelled(activate) -> Succeeded(deactivate)");
    executor.shutdown().await?;
    Ok(())
}

async fn discards_queued(operation: LifecycleOperation) -> Result<()> {
    let fixture = Fixture::new()?;
    let marker = fixture.dir.join("forbidden");
    let activate = fixture.script("activate", &format!("touch '{}'", marker.display()))?;
    let deactivate = fixture.script("deactivate", "exit 0")?;
    let (executor, mut events) =
        fixture.executor(&activate, &deactivate, 1_000, HaHooks::default())?;
    // No yield: both submissions happen before the single worker can dequeue.
    executor.submit(operation, 1, context())?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        2,
        context(),
    )?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Cancelled
    );
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    assert!(!marker.exists());
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_discards_queued_activate_on_deactivate() -> Result<()> {
    discards_queued(LifecycleOperation::Activate(ActivationMode::Cold)).await
}

#[tokio::test]
async fn executor_discards_queued_verify_on_deactivate() -> Result<()> {
    discards_queued(LifecycleOperation::Verify).await
}

async fn stale_completion(operation: LifecycleOperation) -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let script = fixture.script(
        "stale",
        &format!("{gate_script}\nprintf 'db_incarnation=db-123\\n'"),
    )?;
    let passive = fixture.script("passive", "exit 0")?;
    let (executor, mut events) = fixture.executor(&script, &passive, 3_000, HaHooks::default())?;
    executor.submit(operation, 1, context())?;
    let mut stream = started(&listener).await?;
    // Race a zero exit with revocation. Submission synchronously invalidates completion.
    stream.write_all(b"release\n").await?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        2,
        context(),
    )?;
    let stale = event(&mut events).await?;
    assert_eq!(stale.outcome, LifecycleOutcome::Cancelled);
    assert!(!stale.is_current_success(2));
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_stale_completion_never_reports_success() -> Result<()> {
    stale_completion(LifecycleOperation::Activate(ActivationMode::Cold)).await
}

#[tokio::test]
async fn executor_stale_verify_event_never_refreshes_freshness() -> Result<()> {
    stale_completion(LifecycleOperation::Verify).await
}

#[tokio::test]
async fn executor_spawn_error_is_visible() -> Result<()> {
    let fixture = Fixture::new()?;
    let missing = fixture.dir.join("missing").display().to_string();
    let (executor, mut events) = fixture.executor(&missing, &missing, 1_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::SpawnError
    );
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_notifications_do_not_overlap_critical_jobs() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let marker = fixture.dir.join("notify.ran");
    let activate = fixture.script("activate", &gate_script)?;
    let notify = fixture.script("notify", &format!("env > '{}'", marker.display()))?;
    let hooks = HaHooks {
        on_promote: Some(notify),
        ..HaHooks::default()
    };
    let (executor, mut events) = fixture.executor(&activate, &activate, 3_000, hooks)?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let mut stream = started(&listener).await?;
    let mut notification = context();
    notification.notification = Some(HookContext {
        node_id: "node-a".to_owned(),
        group_id: "group-a".to_owned(),
        interface: "lo".to_owned(),
        state: crate::gruezi::ha::HaState::Active,
        previous_state: crate::gruezi::ha::HaState::Standby,
        reason: None,
        priority: 100,
        peer_id: None,
        peer_state: None,
        peer_priority: None,
        last_peer_seen_ms_ago: None,
    });
    executor.submit(
        LifecycleOperation::Notify(HookEvent::Promote),
        1,
        notification,
    )?;
    assert!(!marker.exists());
    stream.write_all(b"release\n").await?;
    assert_eq!(event(&mut events).await?.kind, LifecycleKind::Activate);
    let result = event(&mut events).await?;
    assert_eq!(result.kind, LifecycleKind::Notify(HookEvent::Promote));
    assert_eq!(result.outcome, LifecycleOutcome::Succeeded);
    assert!(marker.exists());
    let notify_env = fs::read_to_string(marker)?;
    for forbidden in [
        "GRUEZI_MARIADB_ADAPTER_CONF",
        "GRUEZI_TRANSFER_ID",
        "GRUEZI_PEER_NONCE",
        "GRUEZI_DB_INCARNATION",
        "GRUEZI_ACTIVATION_MODE",
        "GRUEZI_DEACTIVATE_MODE",
    ] {
        assert!(!notify_env.contains(forbidden));
    }
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_verify_runs_activate_command_only() -> Result<()> {
    let fixture = Fixture::new()?;
    let activate = fixture.script(
        "activate",
        "test \"$GRUEZI_EVENT\" = verify\nprintf 'db_incarnation=db-123\\n'",
    )?;
    let missing = fixture.dir.join("must-not-run").display().to_string();
    let (executor, mut events) =
        fixture.executor(&activate, &missing, 1_000, HaHooks::default())?;
    executor.submit(LifecycleOperation::Verify, 1, context())?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_cancel_grace_kills_process_group() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let pid_file = fixture.dir.join("pid");
    let script = fixture.script("stubborn", &format!(
        "trap '' TERM\nsleep 300 &\nprintf '%s' \"$!\" > '{}'\n{gate_script}\nwhile :; do :; done",
        pid_file.display()
    ))?;
    let passive = fixture.script("passive", "exit 0")?;
    let (executor, mut events) = fixture.executor(&script, &passive, 3_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let mut stream = started(&listener).await?;
    stream.write_all(b"release\n").await?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        2,
        context(),
    )?;
    let cancelled = event(&mut events).await?;
    assert_eq!(cancelled.outcome, LifecycleOutcome::Cancelled);
    assert_eq!(cancelled.exit_code_or_signal, Some(-libc::SIGKILL));
    assert!(cancelled.elapsed >= Duration::from_millis(CANCEL_GRACE_MS));
    gone(&fs::read_to_string(pid_file)?).await?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_cancellation_after_stdout_eof_reaps_cleanly() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let activate = fixture.script("closed-stdout", &format!("exec 1>&-\n{gate_script}"))?;
    let passive = fixture.script("passive", "exit 0")?;
    let (executor, mut events) =
        fixture.executor(&activate, &passive, 3_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let _stream = started(&listener).await?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        2,
        context(),
    )?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Cancelled
    );
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_verify_is_coalesced_and_old_notifications_are_discarded() -> Result<()> {
    let fixture = Fixture::new()?;
    let count = fixture.dir.join("count");
    let script = fixture.script(
        "verify",
        &format!(
            "printf 'run\\n' >> '{}'\nprintf 'db_incarnation=db-123\\n'",
            count.display()
        ),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    let mut notify = context();
    notify.notification = Some(HookContext {
        node_id: "node-a".to_owned(),
        group_id: "group-a".to_owned(),
        interface: "lo".to_owned(),
        state: crate::gruezi::ha::HaState::Active,
        previous_state: crate::gruezi::ha::HaState::Standby,
        reason: None,
        priority: 100,
        peer_id: None,
        peer_state: None,
        peer_priority: None,
        last_peer_seen_ms_ago: None,
    });
    executor.submit(LifecycleOperation::Notify(HookEvent::Promote), 1, notify)?;
    executor.submit(LifecycleOperation::Verify, 2, context())?;
    executor.submit(LifecycleOperation::Verify, 2, context())?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Cancelled
    );
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Cancelled
    );
    let verified = event(&mut events).await?;
    assert_eq!(verified.kind, LifecycleKind::Verify);
    assert!(verified.is_current_success(2));
    assert!(!verified.is_current_success(3));
    assert_eq!(fs::read_to_string(count)?, "run\n");
    assert!(!executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context()
    )?);
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_no_new_privileges_survives_exec() -> Result<()> {
    let fixture = Fixture::new()?;
    let script = fixture.script("privileges", "awk '/^NoNewPrivs:/ { print $2 > \"/dev/stderr\"; found = 1; if ($2 != 1) exit 1 } END { if (!found) exit 1 }' /proc/self/status")?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let result = event(&mut events).await?;
    assert_eq!(result.outcome, LifecycleOutcome::Succeeded);
    assert_eq!(result.stderr_tail, "1");
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_timeout_cleans_descendants_after_leader_exits() -> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.dir.join("pid");
    let (listener, gate_script) = gate().await?;
    let script = fixture.script(
        "leader-exits",
        &format!(
            "sleep 300 &\nprintf '%s' \"$!\" > '{}'\n{gate_script}\nexit 0",
            pid_file.display()
        ),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 500, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    let mut stream = started(&listener).await?;
    stream.write_all(b"release\n").await?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::TimedOut
    );
    gone(&fs::read_to_string(pid_file)?).await?;
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_critical_job_cancels_notification() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let notify = fixture.script("notify", &gate_script)?;
    let critical = fixture.script("critical", "exit 0")?;
    let hooks = HaHooks {
        on_promote: Some(notify),
        ..HaHooks::default()
    };
    let (executor, mut events) = fixture.executor(&critical, &critical, 1_000, hooks)?;
    let mut notification = context();
    notification.notification = Some(HookContext {
        node_id: "node-a".to_owned(),
        group_id: "group-a".to_owned(),
        interface: "lo".to_owned(),
        state: crate::gruezi::ha::HaState::Active,
        previous_state: crate::gruezi::ha::HaState::Standby,
        reason: None,
        priority: 100,
        peer_id: None,
        peer_state: None,
        peer_priority: None,
        last_peer_seen_ms_ago: None,
    });
    executor.submit(
        LifecycleOperation::Notify(HookEvent::Promote),
        1,
        notification,
    )?;
    let _stream = started(&listener).await?;
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Revoke),
        1,
        context(),
    )?;
    let cancelled = event(&mut events).await?;
    assert_eq!(cancelled.kind, LifecycleKind::Notify(HookEvent::Promote));
    assert_eq!(cancelled.outcome, LifecycleOutcome::Cancelled);
    let passive = event(&mut events).await?;
    assert_eq!(passive.kind, LifecycleKind::Deactivate);
    assert_eq!(passive.outcome, LifecycleOutcome::Succeeded);
    executor.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn executor_closes_inherited_descriptors() -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let fixture = Fixture::new()?;
    let file = fs::File::open("/dev/null")?;
    // SAFETY: file is open; F_DUPFD returns a new owned, non-CLOEXEC descriptor.
    let descriptor = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
    anyhow::ensure!(
        descriptor >= 200,
        "duplicate fd: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the checked descriptor was just allocated and has no other owner.
    let inherited = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let script = fixture.script(
        "descriptors",
        &format!("test ! -e /proc/$$/fd/{descriptor}"),
    )?;
    let (executor, mut events) = fixture.executor(&script, &script, 1_000, HaHooks::default())?;
    executor.submit(
        LifecycleOperation::Activate(ActivationMode::Cold),
        1,
        context(),
    )?;
    assert_eq!(
        event(&mut events).await?.outcome,
        LifecycleOutcome::Succeeded
    );
    executor.shutdown().await?;
    drop(inherited);
    Ok(())
}

#[tokio::test]
async fn executor_deactivate_deadline_is_capped_by_withdraw_budget() -> Result<()> {
    let fixture = Fixture::new()?;
    let (listener, gate_script) = gate().await?;
    let script = fixture.script("deactivate", &gate_script)?;
    let (executor, mut events) = fixture.executor(&script, &script, 3_000, HaHooks::default())?;
    let mut job_context = context();
    let submitted = Instant::now();
    job_context.deactivate_deadline = Some(submitted + Duration::from_millis(500));
    executor.submit(
        LifecycleOperation::Deactivate(DeactivationMode::Baseline),
        1,
        job_context,
    )?;
    let _stream = started(&listener).await?;
    let result = event(&mut events).await?;
    assert_eq!(result.outcome, LifecycleOutcome::TimedOut);
    assert!(submitted.elapsed() < Duration::from_secs(2));
    executor.shutdown().await?;
    Ok(())
}
