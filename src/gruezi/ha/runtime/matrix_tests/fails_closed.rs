//! Plan task 6: the worker and startup paths fail closed (GAP-9).
//!
//! Worker rows run the real coordinator over the simulated kernel with a fake
//! required watchdog. A worker that dies outside shutdown (panic, or the task
//! ending without one) must leave the node unowned: authority revoked, a VIP
//! withdrawal attempted, resource state `uncertain`, no re-acquisition and the
//! watchdog unfed. Startup rows prove a residual managed VIP is removed before
//! any acquisition and, with lifecycle configured, that the baseline
//! `Deactivate` runs before any `Activate`.

use super::*;
use crate::{
    config::{LifecycleConfig, WatchdogMode},
    gruezi::{
        addresses::{MAX_WORKER_RESPAWNS, PanicEveryCommand},
        hooks::{
            DeactivationMode, LifecycleOperation,
            scripted::{ScriptedCall, ScriptedLifecycle},
        },
        watchdog::{FakeWatchdog, WatchdogCall, WatchdogFault},
    },
};

const VIP: &str = "10.0.0.10/24";

/// One `available` node whose peer never runs: it promotes on its startup
/// deadline, so the rows reach an owning node without a second coordinator.
struct Lone {
    node: Node,
    watchdog: FakeWatchdog,
    script: Option<Arc<Mutex<ScriptedLifecycle>>>,
    _net: InMemoryNetwork,
    _peer: InMemoryEndpoint,
}

fn boot_lone(residual_vip: bool, lifecycle: bool) -> Lone {
    let (net, endpoint, peer) = InMemoryNetwork::new_pair();
    let mut config = node_config(Side::A, FailurePolicy::Available);
    config.watchdog.mode = WatchdogMode::Required;
    // P + Q + B < W for one address.
    config.watchdog.timeout_ms = 30_000;
    if lifecycle {
        config.lifecycle = Some(LifecycleConfig {
            activate_command: "/usr/lib/gruezi/lifecycle-activate".to_owned(),
            deactivate_command: "/usr/lib/gruezi/lifecycle-deactivate".to_owned(),
            timeout_ms: 1_000,
        });
    }
    let kernel = SimulatedKernel::new_shared();
    if residual_vip {
        SimulatedKernel::insert_address(&kernel, "eth0", VIP, "");
    }
    let watchdog = FakeWatchdog::new(WatchdogFault::None);
    let script = lifecycle.then(ScriptedLifecycle::shared);
    let node = spawn_with_seams(
        Side::A,
        endpoint,
        config,
        super::super::Seams {
            commands: CommandRunner::Simulated(kernel.clone()),
            authority: super::super::authority_commands::AuthorityCommandRunner::System,
            watchdog: WatchdogDevice::Fake(watchdog.clone()),
            lifecycle: script.clone().map_or(
                super::super::LifecycleRunner::System,
                super::super::LifecycleRunner::Scripted,
            ),
        },
        kernel,
    );
    Lone {
        node,
        watchdog,
        script,
        _net: net,
        _peer: peer,
    }
}

impl Lone {
    async fn wait_owned(&self) -> Result<()> {
        for _ in 0..200 {
            if self.node.snapshot().owned {
                return Ok(());
            }
            settle(Duration::from_millis(100)).await;
        }
        Err(anyhow!("lone node never owned"))
    }

    fn keepalives(&self) -> usize {
        self.watchdog
            .calls()
            .iter()
            .filter(|(_, call)| *call == WatchdogCall::KeepAlive)
            .count()
    }

    fn calls_matching(&self, word: &str) -> usize {
        self.node
            .kernel_calls()
            .iter()
            .filter(|call| call.iter().any(|part| part == word))
            .count()
    }

    fn lifecycle_calls(&self) -> Vec<ScriptedCall> {
        self.script.as_ref().map_or_else(Vec::new, |script| {
            script
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .calls
                .clone()
        })
    }
}

/// Kill the resource worker outside shutdown with `inject`, then let the
/// coordinator run several reconcile intervals and assert the fail-closed
/// outcome with evidence printed from the real status and kernel.
async fn worker_death_row(row: &str, inject: impl FnOnce(&mut SimulatedKernel)) -> Result<()> {
    let lone = boot_lone(false, false);
    lone.wait_owned().await?;
    let adds_before = lone.calls_matching("add");
    let dels_before = lone.calls_matching("del");
    let kernel_before = lone.node.kernel_table();
    assert_eq!(kernel_before, vec!["eth0 10.0.0.10/24".to_owned()]);
    print_matrix_state(row, "before-fault", &lone.node);

    inject(
        &mut lone
            .node
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    // A few reconcile intervals: the first one reaches the dead worker.
    settle(dead_window() + Duration::from_secs(1)).await;
    let keepalives_at_detection = lone.keepalives();
    settle(dead_window() * 4).await;

    let status = lone.node.snapshot();
    print_matrix_state(row, "after-fault", &lone.node);
    println!(
        "FAILS_CLOSED[{row}] owned={} authority={} resource={} confirmed={} watchdog={:?} adds_before={adds_before} adds_after={} dels_before={dels_before} dels_after={} keepalives_at_detection={keepalives_at_detection} keepalives_end={}",
        status.owned,
        status.authority.state,
        status.resource.state,
        status.resource.confirmed,
        status.watchdog,
        lone.calls_matching("add"),
        lone.calls_matching("del"),
        lone.keepalives(),
    );

    assert!(!status.owned, "{row}: a node without a worker must not own");
    assert_ne!(
        status.authority.state, "valid",
        "{row}: authority must be revoked"
    );
    assert_eq!(
        status.resource.state, "uncertain",
        "{row}: a dead worker proves nothing about the VIP"
    );
    assert!(
        lone.calls_matching("del") > dels_before,
        "{row}: a VIP withdrawal must be attempted after the worker died"
    );
    assert_eq!(
        lone.calls_matching("add"),
        adds_before,
        "{row}: no re-acquisition or promotion after the worker died"
    );
    assert_eq!(
        lone.keepalives(),
        keepalives_at_detection,
        "{row}: the watchdog must not be fed once the worker is gone"
    );
    lone.node.stop().await
}

#[tokio::test(start_paused = true)]
async fn worker_panic_fails_closed() -> Result<()> {
    worker_death_row("worker_panic", |kernel| kernel.panic_next_command = true).await
}

#[tokio::test(start_paused = true)]
async fn worker_channel_close_fails_closed() -> Result<()> {
    worker_death_row("worker_channel_close", |kernel| {
        if let Some(abort) = kernel.worker_abort.take() {
            abort.abort();
        }
    })
    .await
}

/// A worker failure that never heals must quiesce, not spin (adversarial
/// probe e): with every command panicking, the bounded replacement budget is
/// spent once and the latched `uncertain` state stays put; adverts and
/// shutdown remain responsive because no respawn/fault churn starves them.
#[tokio::test(start_paused = true)]
async fn worker_persistent_failure_bounded_and_quiesced() -> Result<()> {
    let row = "worker_persistent_failure";
    let lone = boot_lone(false, false);
    lone.wait_owned().await?;
    let calls_before = lone.node.kernel_calls().len();
    let dels_before = lone.calls_matching("del");
    let adds_before = lone.calls_matching("add");

    lone.node
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .panic_every_command = PanicEveryCommand::On;
    settle(dead_window() + Duration::from_secs(1)).await;
    let calls_after_churn = lone.node.kernel_calls().len();
    let packets_at_quiescence = lone.node.snapshot().packets_sent;
    settle(dead_window() * 4).await;

    let status = lone.node.snapshot();
    print_matrix_state(row, "quiesced", &lone.node);
    let churn = calls_after_churn.saturating_sub(calls_before);
    println!(
        "FAILS_CLOSED[{row}] churn={churn} budget={} calls_before={calls_before} calls_after_churn={calls_after_churn} calls_end={} dels_before={dels_before} dels_end={} adds_before={adds_before} adds_end={} packets_at_quiescence={packets_at_quiescence} packets_end={}",
        MAX_WORKER_RESPAWNS,
        lone.node.kernel_calls().len(),
        lone.calls_matching("del"),
        lone.calls_matching("add"),
        status.packets_sent,
    );

    assert!(
        !status.owned,
        "{row}: a node with a dead worker must not own"
    );
    assert_ne!(
        status.authority.state, "valid",
        "{row}: authority must be revoked"
    );
    assert_eq!(
        status.resource.state, "uncertain",
        "{row}: latched uncertainty"
    );
    assert!(
        lone.calls_matching("del") > dels_before,
        "{row}: the withdrawal must have been attempted"
    );
    // Bounded churn: the faulting command plus at most one withdrawal attempt
    // per replacement worker, then nothing ever again.
    assert!(
        churn <= 1 + usize::from(MAX_WORKER_RESPAWNS),
        "{row}: worker-loss churn is not bounded: {churn} new kernel calls"
    );
    assert_eq!(
        lone.node.kernel_calls().len(),
        calls_after_churn,
        "{row}: kernel churn continued after the respawn budget was spent"
    );
    assert_eq!(
        lone.calls_matching("add"),
        adds_before,
        "{row}: no re-acquisition"
    );
    assert!(
        status.packets_sent > packets_at_quiescence,
        "{row}: adverts were starved by worker churn"
    );
    lone.node.stop().await
}

fn first_index(calls: &[Vec<String>], word: &str) -> Option<usize> {
    calls
        .iter()
        .position(|call| call.iter().any(|part| part == word))
}

#[tokio::test(start_paused = true)]
async fn startup_removes_residual_vip_before_acquire() -> Result<()> {
    let row = "startup_residual_vip";
    let lone = boot_lone(true, false);
    // Before the worker's first command could run, the node is not owner.
    assert!(!lone.node.snapshot().owned);
    lone.wait_owned().await?;
    let calls = lone.node.kernel_calls();
    println!("FAILS_CLOSED[{row}] kernel_calls={calls:?}");
    let first_del = first_index(&calls, "del").context("startup withdrawal never ran")?;
    let first_add = first_index(&calls, "add").context("acquisition never ran")?;
    assert!(
        first_del < first_add,
        "{row}: residual VIP removal must precede acquisition: {calls:?}"
    );
    lone.node.stop().await
}

#[tokio::test(start_paused = true)]
async fn startup_runs_baseline_deactivate_first() -> Result<()> {
    let row = "startup_baseline_deactivate";
    let lone = boot_lone(true, true);
    lone.wait_owned().await?;
    let lifecycle = lone.lifecycle_calls();
    let kernel = lone.node.kernel_calls();
    println!("FAILS_CLOSED[{row}] lifecycle={lifecycle:?} kernel_calls={kernel:?}");
    let first = lifecycle.first().context("no lifecycle job ran")?;
    assert_eq!(
        first.operation,
        LifecycleOperation::Deactivate(DeactivationMode::Baseline),
        "{row}: the first lifecycle job must be the baseline Deactivate"
    );
    let activate = lifecycle
        .iter()
        .find(|call| matches!(call.operation, LifecycleOperation::Activate(_)))
        .context("activation never ran")?;
    let baseline_done = first.finished.context("baseline never finished")?;
    assert!(
        baseline_done <= activate.started,
        "{row}: Activate started before the baseline finished"
    );
    let first_del = first_index(&kernel, "del").context("residual VIP never removed")?;
    let first_add = first_index(&kernel, "add").context("acquisition never ran")?;
    assert!(first_del < first_add, "{row}: {kernel:?}");
    lone.node.stop().await
}
