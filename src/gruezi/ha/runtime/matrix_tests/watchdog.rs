//! Watchdog rows use the real coordinator and simulated kernel. The outer
//! polling gate freezes just the coordinator, leaving the fake hardware clock
//! independent of it, as with SIGSTOP on a real owner.

use super::*;
use crate::{
    config::{AuthorityConfig, WatchdogMode},
    gruezi::{
        ha::runtime::authority_commands::{
            AuthorityCommandRunner, ScriptedAuthority, ScriptedOutcome,
        },
        watchdog::{FakeWatchdog, WatchdogCall, WatchdogDevice, WatchdogFault},
    },
};
use std::{
    future::{Future, poll_fn},
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

struct WatchdogHarness {
    pair: Harness,
    a: FakeWatchdog,
    b: FakeWatchdog,
    freeze_a: Arc<AtomicBool>,
}

impl WatchdogHarness {
    fn boot(policy: FailurePolicy, fault: WatchdogFault) -> Self {
        Self::boot_mode(policy, fault, WatchdogMode::Required)
    }

    fn boot_mode(policy: FailurePolicy, fault: WatchdogFault, mode: WatchdogMode) -> Self {
        let (net, a, b) = InMemoryNetwork::new_pair();
        let watchdog_a = FakeWatchdog::new(fault);
        let watchdog_b = FakeWatchdog::new(fault);
        let freeze_a = Arc::new(AtomicBool::new(false));
        let mut config_a = node_config(Side::A, policy.clone());
        config_a.watchdog.mode = mode.clone();
        let mut config_b = node_config(Side::B, policy);
        config_b.watchdog.mode = mode;
        let node_a = spawn(Side::A, a, config_a, watchdog_a.clone(), freeze_a.clone());
        let node_b = spawn(
            Side::B,
            b,
            config_b,
            watchdog_b.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        net.capture_kernels(node_a.kernel.clone(), node_b.kernel.clone());
        Self {
            pair: Harness {
                net,
                node_a,
                node_b,
            },
            a: watchdog_a,
            b: watchdog_b,
            freeze_a,
        }
    }

    fn print(&self, row: &str) {
        print_matrix_state(row, "watchdog", &self.pair.node_a);
        print_matrix_state(row, "watchdog", &self.pair.node_b);
        println!(
            "WATCHDOG MATRIX[{row}] A status={:?} calls={:?}; B status={:?} calls={:?}",
            self.pair.node_a.snapshot().watchdog,
            self.a.calls(),
            self.pair.node_b.snapshot().watchdog,
            self.b.calls()
        );
    }
}

fn spawn(
    side: Side,
    endpoint: InMemoryEndpoint,
    mut runtime: HaRuntimeConfig,
    watchdog: FakeWatchdog,
    frozen: Arc<AtomicBool>,
) -> Node {
    let authority = if runtime.failure_policy == FailurePolicy::External {
        runtime.authority = Some(AuthorityConfig {
            check_command: "authority-check".to_owned(),
            fence_command: None,
            interval_ms: Some(ADVERT_INTERVAL_MS),
            grant_ttl_ms: 20_000,
        });
        AuthorityCommandRunner::Scripted(Arc::new(Mutex::new(ScriptedAuthority {
            check: ScriptedOutcome::Exit(side == Side::A),
            ..ScriptedAuthority::default()
        })))
    } else {
        AuthorityCommandRunner::System
    };
    let name = match side {
        Side::A => "node-a",
        Side::B => "node-b",
    };
    let (status_tx, status_rx) = watch::channel(HaStatus::new(
        runtime.node_id.clone(),
        runtime.group_id.clone(),
        runtime.bind.clone(),
        runtime.peer.clone(),
    ));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let kernel = SimulatedKernel::new_shared();
    let task_kernel = kernel.clone();
    let task = tokio::spawn(async move {
        let coordinator = super::super::run_with_watchdog(
            runtime,
            Transport::InMemory(endpoint),
            CommandRunner::Simulated(task_kernel),
            authority,
            WatchdogDevice::Fake(watchdog),
            Some(status_tx),
            async move {
                let _ = shutdown_rx.await;
            },
        );
        tokio::pin!(coordinator);
        poll_fn(|cx| {
            if frozen.load(Ordering::SeqCst) {
                Poll::Pending
            } else {
                coordinator.as_mut().poll(cx)
            }
        })
        .await
    });
    Node {
        name,
        status_rx,
        kernel,
        shutdown_tx: Some(shutdown_tx),
        task,
    }
}

async fn arm_failure(policy: FailurePolicy) -> Result<()> {
    let harness = WatchdogHarness::boot(policy.clone(), WatchdogFault::Open);
    settle(BOOT_WINDOW).await;
    for node in [&harness.pair.node_a, &harness.pair.node_b] {
        assert!(!node.snapshot().owned);
        assert!(
            kernel_add_calls(node).is_empty(),
            "I10: zero address adds after required arm failure"
        );
        assert_eq!(node.kernel_table(), Vec::<String>::new());
        assert!(!node.snapshot().watchdog.armed);
    }
    assert!(
        harness
            .a
            .calls()
            .iter()
            .any(|(_, call)| *call == WatchdogCall::Open)
    );
    harness.print(&format!("{policy:?}/watchdog_arm_fails"));
    harness.pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_watchdog_arm_fails() -> Result<()> {
    arm_failure(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_watchdog_arm_fails() -> Result<()> {
    arm_failure(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_watchdog_arm_fails() -> Result<()> {
    arm_failure(FailurePolicy::External).await
}

#[tokio::test(start_paused = true)]
async fn matrix_watchdog_partial_arm_blocks_every_add_in_both_modes() -> Result<()> {
    for mode in [WatchdogMode::Required, WatchdogMode::Automatic] {
        for fault in [
            WatchdogFault::Capability,
            WatchdogFault::SetTimeout,
            WatchdogFault::Readback,
            WatchdogFault::EffectiveTimeout(11_000),
        ] {
            let harness = WatchdogHarness::boot_mode(FailurePolicy::Available, fault, mode.clone());
            settle(BOOT_WINDOW).await;
            for node in [&harness.pair.node_a, &harness.pair.node_b] {
                assert!(!node.snapshot().owned);
                assert!(!node.snapshot().watchdog.unprotected);
                assert_eq!(kernel_add_calls(node), Vec::<Vec<String>>::new());
            }
            harness.print(&format!("{mode:?}/partial_arm/{fault:?}"));
            harness.pair.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn matrix_watchdog_automatic_conclusively_unarmed_runs_unprotected() -> Result<()> {
    for fault in [WatchdogFault::Open, WatchdogFault::Nowayout] {
        let harness =
            WatchdogHarness::boot_mode(FailurePolicy::Available, fault, WatchdogMode::Automatic);
        settle(BOOT_WINDOW).await;
        let owner = harness.pair.node_a.snapshot();
        assert!(owner.owned);
        assert!(owner.watchdog.unprotected);
        assert!(!owner.watchdog.armed);
        assert_eq!(
            harness.pair.node_a.kernel_table(),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );
        harness.print("automatic/unprotected");
        harness.pair.stop().await?;
    }
    Ok(())
}

async fn freeze_owner(policy: FailurePolicy) -> Result<()> {
    let mut harness = WatchdogHarness::boot(policy.clone(), WatchdogFault::None);
    settle(BOOT_WINDOW).await;
    assert!(harness.pair.node_a.snapshot().owned);
    assert!(harness.pair.node_a.snapshot().watchdog.armed);
    let before = harness.a.calls();
    assert!(
        before
            .iter()
            .any(|(_, call)| *call == WatchdogCall::KeepAlive)
    );
    let deadline = harness
        .a
        .lock()
        .reset_deadline
        .context("owner reset deadline")?;
    harness.freeze_a.store(true, Ordering::SeqCst);
    settle(Duration::from_secs(5)).await;
    assert_eq!(
        before,
        harness.a.calls(),
        "a frozen coordinator cannot keep feeding"
    );
    if policy == FailurePolicy::Available {
        assert!(harness.pair.node_b.snapshot().owned);
        assert_ne!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
        println!(
            "WATCHDOG available freeze: takeover precedes W; residual VIP overlap is expected"
        );
    } else {
        assert!(
            !harness.pair.node_b.snapshot().owned,
            "strict silence grants nothing"
        );
    }
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    settle(remaining).await;
    let calls = harness.a.calls();
    assert_eq!(calls.len(), before.len() + 1);
    assert_eq!(
        calls.last().map(|(_, call)| call),
        Some(&WatchdogCall::Reset { deadline })
    );
    assert!(!calls.iter().any(|(_, call)| matches!(
        call,
        WatchdogCall::Disarm | WatchdogCall::MagicClose | WatchdogCall::Close
    )));
    harness.print(&format!("{policy:?}/freeze_self_fence"));
    println!(
        "WATCHDOG reset reached on fake clock deadline={deadline:?}; coordinator never resumed"
    );
    harness.pair.node_a.crash();
    harness.pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_freeze_self_fence() -> Result<()> {
    freeze_owner(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_freeze_self_fence() -> Result<()> {
    freeze_owner(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_watchdog_unmanaged_owner_feeds_without_reset() -> Result<()> {
    let (_net, endpoint, _peer) = InMemoryNetwork::new_pair();
    let mut runtime = node_config(Side::A, FailurePolicy::Available);
    runtime.addresses.clear();
    runtime.watchdog.mode = WatchdogMode::Required;
    let fake = FakeWatchdog::new(WatchdogFault::None);
    let node = spawn(
        Side::A,
        endpoint,
        runtime,
        fake.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    settle(BOOT_WINDOW).await;
    assert!(node.snapshot().owned);
    assert!(node.snapshot().watchdog.armed);
    assert_eq!(node.snapshot().resource.state, "unmanaged");
    let first_deadline = fake.lock().reset_deadline.context("armed reset deadline")?;
    let mut previous_feeds = fake
        .calls()
        .iter()
        .filter(|(_, call)| *call == WatchdogCall::KeepAlive)
        .count();
    assert!(previous_feeds > 0);
    // Each advert wakes the actual coordinator. Run for three full effective
    // timeouts so merely arming, or a single feed on promotion, cannot pass.
    for _ in 0..45 {
        settle(Duration::from_secs(1)).await;
        let calls = fake.calls();
        let feeds = calls
            .iter()
            .filter(|(_, call)| *call == WatchdogCall::KeepAlive)
            .count();
        assert!(
            feeds > previous_feeds,
            "each coordinator interval must feed unmanaged ownership"
        );
        previous_feeds = feeds;
        assert!(
            !calls
                .iter()
                .any(|(_, call)| matches!(call, WatchdogCall::Reset { .. }))
        );
        assert!(
            fake.lock()
                .reset_deadline
                .is_some_and(|deadline| deadline > tokio::time::Instant::now())
        );
        assert!(node.snapshot().owned);
        assert!(node.snapshot().watchdog.armed);
    }
    assert!(tokio::time::Instant::now() > first_deadline);
    assert_eq!(node.kernel_calls(), Vec::<Vec<String>>::new());
    let current_deadline = fake.lock().reset_deadline;
    let calls = fake.calls();
    println!(
        "WATCHDOG QA UNMANAGED owned={} armed={} first_deadline={first_deadline:?} current_deadline={:?} feeds={previous_feeds} reset_events=0 calls={:?}",
        node.snapshot().owned,
        node.snapshot().watchdog.armed,
        current_deadline,
        calls,
    );
    node.stop().await
}
