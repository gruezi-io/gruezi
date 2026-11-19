//! Deterministic failure-matrix harness (plan todo 10).
//!
//! Two in-process nodes run the real HA control loop over the in-memory
//! transport with the simulated kernel as the command runner, all under
//! `tokio::time::pause` - no sockets, ports, subprocesses or real time. The
//! tests are named `matrix_strict_*` and `matrix_available_*`, seven rows per
//! policy, and every one of them prints the elected role, the authority
//! (state/source/term/expiry), the safety level and the kernel table plus the
//! kernel call log, so a "green" run shows the kernel and authority state and
//! never just test output (invariants I1, I2, I3, I5).

use super::{
    Transport,
    in_memory::{DropRule, InMemoryEndpoint, InMemoryNetwork, SentAdvert, Side, decode_advert},
    run_with_transport,
};
use crate::{
    config::{FailurePolicy, WatchdogConfig},
    gruezi::{
        addresses::{CommandGate, CommandRunner, SimulatedKernel},
        ha::{
            HaAuth, HaDecisionReason, HaRuntimeConfig, HaStatus,
            protocol::{HaPacket, TransferPhase},
            status::{SafetyLevel, TransitionTrigger},
        },
        hooks::HaHooks,
        ownership::AuthoritySource,
    },
};
use anyhow::{Context, Result, anyhow};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::Duration,
};

const ADVERT_INTERVAL_MS: u64 = 1_000;
const DEAD_FACTOR: u8 = 3;
const HOLD_DOWN_MS: u64 = 1_000;
/// Window chosen well past session establishment (~2 intervals), consent (~3
/// intervals) and a full acquire round trip.
const BOOT_WINDOW: Duration = Duration::from_millis(8_000);

/// `advert_interval_ms * dead_factor`: the bilateral consent window and the
/// core of the follower deadline (which adds `hold_down_ms`).
fn dead_window() -> Duration {
    Duration::from_millis(ADVERT_INTERVAL_MS * u64::from(DEAD_FACTOR))
}

fn in_memory_addr(side: Side) -> std::net::SocketAddr {
    match side {
        Side::A => super::in_memory::addr_a(),
        Side::B => super::in_memory::addr_b(),
    }
}

fn node_config(side: Side, policy: FailurePolicy) -> HaRuntimeConfig {
    let (node_id, peer_id, priority) = match side {
        Side::A => ("node-a", "node-b", 110_u8),
        Side::B => ("node-b", "node-a", 100_u8),
    };
    HaRuntimeConfig {
        node_id: node_id.to_owned(),
        group_id: "cluster-ha".to_owned(),
        bind: in_memory_addr(side).to_string(),
        peer: in_memory_addr(side.inbound()).to_string(),
        interface: "eth0".to_owned(),
        addresses: vec!["10.0.0.10/24".to_owned()],
        peer_id: peer_id.to_owned(),
        failure_policy: policy,
        withdraw_timeout_ms: 5_000,
        command_timeout_ms: 3_000,
        watchdog: WatchdogConfig::default(),
        authority: None,
        priority,
        // Stable failover: recovery rows assert the rank winner ends up as the
        // single owner, not that a returning node steals ownership.
        preempt: false,
        advert_interval_ms: ADVERT_INTERVAL_MS,
        dead_factor: DEAD_FACTOR,
        hold_down_ms: HOLD_DOWN_MS,
        jitter_ms: 0,
        auth: HaAuth::SharedKey {
            key: "shared-secret-for-tests".to_owned(),
        },
        hooks: HaHooks::default(),
        ip_command: "ip".to_owned(),
        arping_command: "arping".to_owned(),
        ndsend_command: "ndsend".to_owned(),
    }
}

struct Node {
    name: &'static str,
    status_rx: watch::Receiver<HaStatus>,
    kernel: Arc<Mutex<SimulatedKernel>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl Node {
    fn snapshot(&self) -> HaStatus {
        self.status_rx.borrow().clone()
    }

    fn kernel_table(&self) -> Vec<String> {
        SimulatedKernel::table_snapshot(&self.kernel)
    }

    fn kernel_calls(&self) -> Vec<Vec<String>> {
        SimulatedKernel::calls_snapshot(&self.kernel)
    }

    /// Crash semantics: the process dies without cleanup, so no withdrawal
    /// runs and any residual VIP stays in the kernel (as after SIGKILL).
    fn crash(&mut self) {
        self.task.abort();
    }

    fn begin_shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }

    async fn stop(mut self) -> Result<()> {
        self.begin_shutdown();
        match self.task.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(anyhow!("node task failed: {error}")),
        }
    }
}

struct Harness {
    net: InMemoryNetwork,
    node_a: Node,
    node_b: Node,
}

impl Harness {
    fn boot(policy: FailurePolicy) -> Self {
        Self::boot_configs(
            node_config(Side::A, policy.clone()),
            node_config(Side::B, policy),
        )
    }

    fn boot_configs(config_a: HaRuntimeConfig, config_b: HaRuntimeConfig) -> Self {
        let (net, endpoint_a, endpoint_b) = InMemoryNetwork::new_pair();
        let node_a = spawn_config(Side::A, endpoint_a, config_a, SimulatedKernel::new_shared());
        let node_b = spawn_config(Side::B, endpoint_b, config_b, SimulatedKernel::new_shared());
        net.capture_kernels(node_a.kernel.clone(), node_b.kernel.clone());
        Self {
            net,
            node_a,
            node_b,
        }
    }

    async fn stop(self) -> Result<()> {
        self.node_a.stop().await?;
        self.node_b.stop().await
    }

    async fn restart(&mut self, side: Side, policy: FailurePolicy, preempt: bool) {
        let node = match side {
            Side::A => &mut self.node_a,
            Side::B => &mut self.node_b,
        };
        node.crash();
        settle(Duration::ZERO).await;
        let mut config = node_config(side, policy);
        config.preempt = preempt;
        *node = spawn_config(
            side,
            self.net.restart_endpoint(side),
            config,
            node.kernel.clone(),
        );
    }
}

fn spawn_config(
    side: Side,
    endpoint: InMemoryEndpoint,
    runtime: HaRuntimeConfig,
    kernel: Arc<Mutex<SimulatedKernel>>,
) -> Node {
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
    let task = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            run_with_transport(
                runtime,
                Transport::InMemory(endpoint),
                CommandRunner::Simulated(kernel),
                Some(status_tx),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        })
    };
    Node {
        name,
        status_rx,
        kernel,
        shutdown_tx: Some(shutdown_tx),
        task,
    }
}

/// Advance virtual time and let every task drain to its next idle point.
/// Under `start_paused = true` no wall-clock time passes.
async fn settle(duration: Duration) {
    tokio::time::sleep(duration).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

fn kernel_add_calls(node: &Node) -> Vec<Vec<String>> {
    node.kernel_calls()
        .into_iter()
        .filter(|call| call.iter().any(|part| part == "add"))
        .collect()
}

/// One evidence line per node: elected role, ownership, authority, safety and
/// the kernel truth (table plus the full command log).
fn print_matrix_state(row: &str, phase: &str, node: &Node) {
    let status = node.snapshot();
    println!(
        "MATRIX[{row}] {phase} node={} state={:?} owned={} authority={} source={:?} term={} expires_in_ms={:?} safety={:?} resource={} kernel_table={:?} kernel_calls={:?}",
        node.name,
        status.state,
        status.owned,
        status.authority.state,
        status.authority.source,
        status.authority.term,
        status.authority.expires_in_ms,
        status.safety.level,
        status.resource.state,
        node.kernel_table(),
        node.kernel_calls(),
    );
}

fn assert_single_owner(row: &str, owner: &Node, other: &Node) {
    let status = owner.snapshot();
    let other_status = other.snapshot();
    assert!(
        status.owned,
        "{row}: exactly one owner expected on {}",
        owner.name
    );
    assert_eq!(
        status.authority.source,
        Some(AuthoritySource::Peer),
        "{row}: owner authority must come from bilateral consent"
    );
    assert!(
        status.authority.expires_in_ms.unwrap_or(0) > 0,
        "{row}: consent must be fresh (expires_in_ms set)"
    );
    assert!(!other_status.owned, "{row}: {} must not own", other.name);
    assert_eq!(
        other.kernel_table(),
        Vec::<String>::new(),
        "{row}: {} kernel must stay empty",
        other.name
    );
    let owners = u8::from(status.owned) + u8::from(other_status.owned);
    println!(
        "MATRIX[{row}] RESULT owners={owners} owner={} authority.source={:?} consent_fresh={} network=in-memory kernel_owner={:?} kernel_other={:?}",
        owner.name,
        status.authority.source,
        status.authority.expires_in_ms.unwrap_or(0) > 0,
        owner.kernel_table(),
        other.kernel_table(),
    );
}

/// I1: the rank loser never touches the kernel; the rank winner acquires only
/// after session, consent and the bootstrap rule are all satisfied.
async fn run_startup(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "startup");
    let harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    print_matrix_state(row, "settled", &harness.node_a);
    print_matrix_state(row, "settled", &harness.node_b);
    assert_single_owner(row, &harness.node_a, &harness.node_b);
    assert_eq!(
        harness.node_a.kernel_table(),
        vec!["eth0 10.0.0.10/24".to_owned()],
        "{row}: rank winner confirmed the VIP in its kernel"
    );
    harness.stop().await
}

/// I2: a crashed active node leaves silence, and silence never authorizes a
/// takeover in strict mode - for fake time advanced far beyond the dead
/// window (10x). The standby is elected by the local timeout (existing
/// election rules) but stays unauthorized and never acquires (I1).
async fn run_active_crash(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "active_crash");
    let strict = policy == FailurePolicy::Strict;
    let mut harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.node_a.crash();
    settle(dead_window() * 10).await;
    print_matrix_state(row, "after 10x dead window", &harness.node_b);
    let standby = harness.node_b.snapshot();
    if strict {
        assert!(
            !standby.owned,
            "{row}: strict must never promote on silence"
        );
        assert_ne!(
            standby.authority.state, "valid",
            "{row}: no authority on silence"
        );
        assert!(
            kernel_add_calls(&harness.node_b).is_empty(),
            "{row}: elected-but-unauthorized never acquired (I1/I2); add calls: {:?}",
            kernel_add_calls(&harness.node_b)
        );
        println!(
            "MATRIX[{row}] RESULT NO TAKEOVER after {}ms fake time (10x dead window); node-b add_calls=0 owned=false authority={}",
            (dead_window() * 10).as_millis(),
            standby.authority.state
        );
    } else {
        assert!(
            standby.owned,
            "{row}: available must promote after the deadline"
        );
        assert_eq!(standby.authority.source, Some(AuthoritySource::Assumed));
        assert_eq!(standby.safety.level, SafetyLevel::Assumed);
        assert_eq!(
            harness.node_b.kernel_table(),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );
        println!(
            "MATRIX[{row}] RESULT node-b promoted source=assumed safety=assumed kernel={:?}",
            harness.node_b.kernel_table()
        );
    }
    harness.stop().await
}

/// I3: a crashed standby silently stops acknowledging, which stops the
/// owner's consent advancing. Strict ends authorized ownership within the
/// consent window plus the withdrawal deadline (checked well inside that
/// bound); available keeps the owner running but with `assumed` authority and
/// `assumed` safety.
async fn run_standby_crash(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "standby_crash");
    let strict = policy == FailurePolicy::Strict;
    let mut harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.node_b.crash();
    if strict {
        // Bound: consent window + withdrawal deadline. The check happens
        // after the consent window plus half a second - well inside the
        // withdrawal deadline - so ownership must already be gone.
        settle(dead_window() + Duration::from_millis(500)).await;
        print_matrix_state(row, "at consent window + 500ms", &harness.node_a);
        let owner = harness.node_a.snapshot();
        assert!(
            !owner.owned,
            "{row}: strict owner must withdraw on consent loss"
        );
        assert_ne!(owner.authority.state, "valid", "{row}: authority revoked");
        assert_eq!(
            harness.node_a.kernel_table(),
            Vec::<String>::new(),
            "{row}: VIP withdrawn and absence confirmed (I3)"
        );
        assert_eq!(
            owner.last_transition_reason,
            Some(HaDecisionReason::ConsentExpired)
        );
        assert_eq!(
            owner.last_transition.as_ref().map(|t| t.trigger),
            Some(TransitionTrigger::Consent)
        );
        println!(
            "MATRIX[{row}] RESULT ownership ended within consent window + withdrawal deadline; last_transition={:?} kernel={:?}",
            owner.last_transition,
            harness.node_a.kernel_table()
        );
    } else {
        settle(Duration::from_millis(250)).await;
        assert!(
            harness.node_a.snapshot().owned,
            "{row}: owner continues right after the crash"
        );
        settle(dead_window() + Duration::from_millis(500)).await;
        print_matrix_state(row, "at consent window + 500ms", &harness.node_a);
        let owner = harness.node_a.snapshot();
        assert!(owner.owned, "{row}: available owner continues");
        assert_eq!(owner.authority.source, Some(AuthoritySource::Assumed));
        assert_eq!(owner.safety.level, SafetyLevel::Assumed);
        assert_eq!(
            harness.node_a.kernel_table(),
            vec!["eth0 10.0.0.10/24".to_owned()]
        );
        println!(
            "MATRIX[{row}] RESULT owner continues source=assumed safety=assumed kernel={:?}",
            harness.node_a.kernel_table()
        );
    }
    harness.stop().await
}

/// Full partition (loss both ways). Strict: I2 + I3 together - the owner
/// withdraws and the standby never promotes, so the resource is unavailable
/// but never dual-owned. Available: I5 - both nodes own with `assumed`
/// authority and `assumed` safety (documented dual ownership).
async fn run_full_partition(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "full_partition");
    let strict = policy == FailurePolicy::Strict;
    let harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.net.set_drop(Side::A, DropRule::All);
    harness.net.set_drop(Side::B, DropRule::All);
    settle(dead_window() * 10).await;
    print_matrix_state(row, "partitioned 10x dead window", &harness.node_a);
    print_matrix_state(row, "partitioned 10x dead window", &harness.node_b);
    let a = harness.node_a.snapshot();
    let b = harness.node_b.snapshot();
    if strict {
        assert!(!a.owned, "{row}: strict owner must withdraw (I3)");
        assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
        assert!(
            !b.owned,
            "{row}: strict standby must never promote on silence (I2)"
        );
        assert!(
            kernel_add_calls(&harness.node_b).is_empty(),
            "{row}: standby never acquired (I1/I2); add calls: {:?}",
            kernel_add_calls(&harness.node_b)
        );
        println!(
            "MATRIX[{row}] RESULT owners=0 (unavailable, never dual-owned) kernel_a={:?} kernel_b={:?}",
            harness.node_a.kernel_table(),
            harness.node_b.kernel_table()
        );
    } else {
        assert!(
            a.owned && b.owned,
            "{row}: available accepts two owners (I5)"
        );
        assert_eq!(a.authority.source, Some(AuthoritySource::Assumed));
        assert_eq!(b.authority.source, Some(AuthoritySource::Assumed));
        assert_eq!(a.safety.level, SafetyLevel::Assumed);
        assert_eq!(b.safety.level, SafetyLevel::Assumed);
        println!(
            "MATRIX[{row}] RESULT owners=2 node-a source={:?} safety={:?} node-b source={:?} safety={:?} kernel_a={:?} kernel_b={:?}",
            a.authority.source,
            a.safety.level,
            b.authority.source,
            b.safety.level,
            harness.node_a.kernel_table(),
            harness.node_b.kernel_table()
        );
    }
    harness.stop().await
}

/// One-way loss A->B: the standby hears nothing from the active node.
/// Strict: the owner's consent freezes because the peer can no longer
/// acknowledge new sends (I3), and the standby never promotes (I2).
/// Available: the standby may promote while the owner keeps serving.
async fn run_one_way_loss_a_to_b(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "one_way_loss_a_to_b");
    let strict = policy == FailurePolicy::Strict;
    let harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.net.set_drop(Side::A, DropRule::All);
    settle(dead_window() * 10).await;
    print_matrix_state(row, "A->B loss 10x dead window", &harness.node_a);
    print_matrix_state(row, "A->B loss 10x dead window", &harness.node_b);
    let a = harness.node_a.snapshot();
    let b = harness.node_b.snapshot();
    if strict {
        assert!(!a.owned, "{row}: consent must lapse without acks (I3)");
        assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
        assert!(!b.owned, "{row}: strict must never promote on silence (I2)");
        assert!(
            kernel_add_calls(&harness.node_b).is_empty(),
            "{row}: standby never acquired (I1/I2); add calls: {:?}",
            kernel_add_calls(&harness.node_b)
        );
        println!(
            "MATRIX[{row}] RESULT owners=0 consent lapsed owner withdrawn; standby never promoted"
        );
    } else {
        // The existing conflict-yield election rule demotes the owner once it
        // observes the standby's promotion, so availability is preserved with
        // exactly one owner. (The spec-7 prose suggests dual ownership here;
        // the documented dual-owner case is I5, the full partition row.)
        assert!(!a.owned, "{row}: owner yielded to the promoted claimant");
        assert!(b.owned, "{row}: standby promoted");
        assert_eq!(b.authority.source, Some(AuthoritySource::Assumed));
        assert_eq!(b.safety.level, SafetyLevel::Assumed);
        assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
        println!(
            "MATRIX[{row}] RESULT owners=1 owner=node-b source=assumed (conflict-yield convergence) kernel_a={:?} kernel_b={:?}",
            harness.node_a.kernel_table(),
            harness.node_b.kernel_table()
        );
    }
    harness.stop().await
}

/// One-way loss B->A: the owner hears nothing, so no fresh advert carries an
/// acknowledgement and consent stops advancing. Strict: the owner withdraws
/// and the healthy standby (which still sees the owner advertise) never
/// promotes (I3). Available: the owner continues with `assumed` authority and
/// the standby holds - one owner.
async fn run_one_way_loss_b_to_a(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "one_way_loss_b_to_a");
    let strict = policy == FailurePolicy::Strict;
    let harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.net.set_drop(Side::B, DropRule::All);
    settle(dead_window() * 10).await;
    print_matrix_state(row, "B->A loss 10x dead window", &harness.node_a);
    print_matrix_state(row, "B->A loss 10x dead window", &harness.node_b);
    let a = harness.node_a.snapshot();
    let b = harness.node_b.snapshot();
    if strict {
        assert!(!a.owned, "{row}: consent must lapse without acks (I3)");
        assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
        assert!(!b.owned, "{row}: nobody promotes in strict (I2)");
        assert!(
            kernel_add_calls(&harness.node_b).is_empty(),
            "{row}: standby never acquired (I1/I2); add calls: {:?}",
            kernel_add_calls(&harness.node_b)
        );
        println!(
            "MATRIX[{row}] RESULT owners=0 consent lapsed owner withdrawn; standby never promoted"
        );
    } else {
        assert!(a.owned, "{row}: available owner continues");
        assert_eq!(a.authority.source, Some(AuthoritySource::Assumed));
        assert!(!b.owned, "{row}: standby sees the live owner and holds");
        println!(
            "MATRIX[{row}] RESULT owners=1 owner=node-a source=assumed kernel={:?}",
            harness.node_a.kernel_table()
        );
    }
    harness.stop().await
}

/// Recovery/failback after a full partition: once both directions carry
/// traffic again, the session re-establishes, consent resumes and the rank
/// winner re-acquires through the bootstrap rule. Exactly one owner, sourced
/// from bilateral consent.
async fn run_recovery_failback(policy: FailurePolicy) -> Result<()> {
    let row = row_name(&policy, "recovery_failback");
    let strict = policy == FailurePolicy::Strict;
    let harness = Harness::boot(policy);
    settle(BOOT_WINDOW).await;
    assert!(
        harness.node_a.snapshot().owned,
        "{row}: setup needs an owner"
    );
    harness.net.set_drop(Side::A, DropRule::All);
    harness.net.set_drop(Side::B, DropRule::All);
    settle(dead_window() * 10).await;
    let (a_partitioned, b_partitioned) = (harness.node_a.snapshot(), harness.node_b.snapshot());
    if strict {
        assert!(
            !a_partitioned.owned && !b_partitioned.owned,
            "{row}: strict partition state"
        );
    } else {
        assert!(
            a_partitioned.owned && b_partitioned.owned,
            "{row}: available partition state"
        );
    }
    // Heal direction by direction (A->B first, then B->A) so the recovery
    // outcome does not depend on the two loops' poll order.
    harness.net.set_drop(Side::A, DropRule::None);
    settle(dead_window() * 2).await;
    harness.net.set_drop(Side::B, DropRule::None);
    settle(dead_window() * 3).await;
    print_matrix_state(row, "recovered", &harness.node_a);
    print_matrix_state(row, "recovered", &harness.node_b);
    assert_single_owner(row, &harness.node_a, &harness.node_b);
    assert_eq!(
        harness.node_b.kernel_table(),
        Vec::<String>::new(),
        "{row}: rank loser's VIP is gone after recovery"
    );
    harness.stop().await
}

fn row_name(policy: &FailurePolicy, row: &str) -> &'static str {
    match (policy, row) {
        (FailurePolicy::Strict, "startup") => "strict/startup",
        (FailurePolicy::Strict, "active_crash") => "strict/active_crash",
        (FailurePolicy::Strict, "standby_crash") => "strict/standby_crash",
        (FailurePolicy::Strict, "full_partition") => "strict/full_partition",
        (FailurePolicy::Strict, "one_way_loss_a_to_b") => "strict/one_way_loss_a_to_b",
        (FailurePolicy::Strict, "one_way_loss_b_to_a") => "strict/one_way_loss_b_to_a",
        (FailurePolicy::Strict, "recovery_failback") => "strict/recovery_failback",
        (FailurePolicy::Available, "startup") => "available/startup",
        (FailurePolicy::Available, "active_crash") => "available/active_crash",
        (FailurePolicy::Available, "standby_crash") => "available/standby_crash",
        (FailurePolicy::Available, "full_partition") => "available/full_partition",
        (FailurePolicy::Available, "one_way_loss_a_to_b") => "available/one_way_loss_a_to_b",
        (FailurePolicy::Available, "one_way_loss_b_to_a") => "available/one_way_loss_b_to_a",
        (FailurePolicy::Available, "recovery_failback") => "available/recovery_failback",
        _ => "unknown",
    }
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_startup() -> Result<()> {
    run_startup(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_active_crash() -> Result<()> {
    run_active_crash(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_standby_crash() -> Result<()> {
    run_standby_crash(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_full_partition() -> Result<()> {
    run_full_partition(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_one_way_loss_a_to_b() -> Result<()> {
    run_one_way_loss_a_to_b(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_one_way_loss_b_to_a() -> Result<()> {
    run_one_way_loss_b_to_a(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_recovery_failback() -> Result<()> {
    run_recovery_failback(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_startup() -> Result<()> {
    // The available policy must warn at startup that it can dual-own; the
    // captured log proves the warning is actually emitted.
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    // A process-global subscriber keeps every thread on the same callsite
    // interest: a thread-local one lets sibling tests without a subscriber
    // disable the warn callsite before this node emits it.
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| anyhow!("failed to install log capture: {error}"))?;
    tracing::callsite::rebuild_interest_cache();
    run_startup(FailurePolicy::Available).await?;
    let logs = capture.contents();
    assert!(
        logs.contains("dual-own"),
        "available startup must warn that it can dual-own; captured logs: {logs}"
    );
    println!("MATRIX[available/startup] STARTUP WARNING CAPTURED: dual-own");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn matrix_available_active_crash() -> Result<()> {
    run_active_crash(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_standby_crash() -> Result<()> {
    run_standby_crash(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_full_partition() -> Result<()> {
    run_full_partition(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_one_way_loss_a_to_b() -> Result<()> {
    run_one_way_loss_a_to_b(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_one_way_loss_b_to_a() -> Result<()> {
    run_one_way_loss_b_to_a(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_recovery_failback() -> Result<()> {
    run_recovery_failback(FailurePolicy::Available).await
}

fn adverts(net: &InMemoryNetwork, side: Side, phase: TransferPhase) -> Vec<(SentAdvert, HaPacket)> {
    net.history()
        .into_iter()
        .filter_map(|sent| {
            let packet = decode_advert(&sent.bytes)?;
            (sent.side == side && packet.transfer_phase == phase).then_some((sent, packet))
        })
        .collect()
}

/// I6 and the successor's symmetric presence gate are checked against the
/// kernel at SEND time, including datagrams deliberately lost by the fabric.
fn assert_advert_order(net: &InMemoryNetwork) {
    for sent in net.history() {
        let Some(packet) = decode_advert(&sent.bytes) else {
            continue;
        };
        let table = match sent.side {
            Side::A => &sent.kernel_a,
            Side::B => &sent.kernel_b,
        };
        match packet.transfer_phase {
            TransferPhase::Released | TransferPhase::Returned => {
                assert!(
                    table.is_empty(),
                    "I6: {:?} before absence: {sent:?}",
                    packet.transfer_phase
                );
                assert!(matches!(
                    packet.resource_state,
                    super::super::protocol::AdvertResourceState::Absent
                        | super::super::protocol::AdvertResourceState::Unmanaged
                ));
                assert_eq!(
                    packet.authority_source,
                    super::super::protocol::AdvertAuthoritySource::None
                );
            }
            TransferPhase::Owned => {
                assert_eq!(table, &vec!["eth0 10.0.0.10/24".to_owned()]);
                assert_eq!(
                    packet.resource_state,
                    super::super::protocol::AdvertResourceState::Present
                );
                assert_ne!(
                    packet.authority_source,
                    super::super::protocol::AdvertAuthoritySource::None
                );
            }
            TransferPhase::None | TransferPhase::Requesting => {}
        }
    }
}

fn print_transfer_sequence(row: &str, net: &InMemoryNetwork) {
    let start = net.history().first().map(|sent| sent.at);
    for sent in net.history() {
        let Some(packet) = decode_advert(&sent.bytes) else {
            continue;
        };
        if packet.transfer_phase != TransferPhase::None {
            println!(
                "HANDOFF[{row}] t={}ms side={:?} seq={} transfer_id={} phase={} resource={:?} kernel_a={:?} kernel_b={:?}",
                start.map_or(0, |start| sent.at.duration_since(start).as_millis()),
                sent.side,
                packet.seq,
                packet.transfer_id,
                format!("{:?}", packet.transfer_phase).to_ascii_lowercase(),
                packet.resource_state,
                sent.kernel_a,
                sent.kernel_b
            );
        }
    }
}

async fn run_graceful_failover(policy: FailurePolicy) -> Result<()> {
    let mut harness = Harness::boot(policy.clone());
    settle(BOOT_WINDOW).await;
    assert_single_owner("handoff setup", &harness.node_a, &harness.node_b);
    // A gated delete proves that no Released can escape while withdrawal
    // runs, rather than observing only the final successful snapshot.
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    harness
        .node_a
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gate_next_del = Some(CommandGate {
        entered: entered_tx,
        release: release_rx,
    });
    harness.node_a.begin_shutdown();
    entered_rx.await.context("withdrawal must enter the gate")?;
    settle(Duration::from_millis(ADVERT_INTERVAL_MS)).await;
    assert!(adverts(&harness.net, Side::A, TransferPhase::Released).is_empty());
    assert_eq!(kernel_add_calls(&harness.node_b), Vec::<Vec<String>>::new());
    println!(
        "HANDOFF[{policy:?}] BEFORE release: withdrawal gated kernel_a={:?} kernel_b={:?}; released_adverts=0",
        harness.node_a.kernel_table(),
        harness.node_b.kernel_table()
    );
    let _ = release_tx.send(Ok(()));
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 2)).await;
    assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
    assert!(harness.node_b.snapshot().owned);
    assert_eq!(
        harness.node_b.kernel_table(),
        vec!["eth0 10.0.0.10/24".to_owned()]
    );
    let released = adverts(&harness.net, Side::A, TransferPhase::Released);
    let owned = adverts(&harness.net, Side::B, TransferPhase::Owned);
    let (_, release) = released.first().context("A must advertise Released")?;
    let (_, acquired) = owned.first().context("B must advertise Owned")?;
    assert_ne!(release.transfer_id, 0);
    assert_eq!(release.transfer_id, acquired.transfer_id);
    assert_advert_order(&harness.net);
    print_transfer_sequence("graceful", &harness.net);
    println!(
        "HANDOFF[{policy:?}] RESULT A absent BEFORE phase=released; B present BEFORE phase=owned"
    );
    harness.stop().await
}

async fn run_failed_withdrawal(policy: FailurePolicy) -> Result<()> {
    let mut harness = Harness::boot(policy.clone());
    settle(BOOT_WINDOW).await;
    {
        let mut kernel = harness
            .node_a
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Both a residual VIP and a failed read-back must forbid release.
        kernel.fail_del_addresses.push("10.0.0.10/24".to_owned());
        kernel.fail_show = true;
    }
    harness.node_a.begin_shutdown();
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 3)).await;
    print_matrix_state("failed_withdrawal", "read-back failed", &harness.node_a);
    print_matrix_state("failed_withdrawal", "successor blocked", &harness.node_b);
    print_transfer_sequence("failed_withdrawal", &harness.net);
    assert!(adverts(&harness.net, Side::A, TransferPhase::Released).is_empty());
    assert_ne!(harness.node_a.kernel_table(), Vec::<String>::new());
    assert_eq!(harness.node_b.kernel_table(), Vec::<String>::new());
    assert_eq!(kernel_add_calls(&harness.node_b), Vec::<Vec<String>>::new());
    assert!(!harness.node_a.snapshot().owned);
    assert!(!harness.node_b.snapshot().owned);
    println!(
        "HANDOFF[{policy:?}] RESULT read-back failed; released_adverts=0; successor_adds=0; kernel_a={:?} kernel_b={:?}",
        harness.node_a.kernel_table(),
        harness.node_b.kernel_table()
    );
    {
        let mut kernel = harness
            .node_a
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        kernel.fail_show = false;
        kernel.fail_del_addresses.clear();
    }
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_graceful_failover() -> Result<()> {
    run_graceful_failover(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_graceful_failover() -> Result<()> {
    run_graceful_failover(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_strict_failed_withdrawal() -> Result<()> {
    run_failed_withdrawal(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_available_failed_withdrawal() -> Result<()> {
    run_failed_withdrawal(FailurePolicy::Available).await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_lost_release_is_periodically_retransmitted() -> Result<()> {
    let mut harness = Harness::boot(FailurePolicy::Strict);
    settle(BOOT_WINDOW).await;
    harness
        .net
        .set_drop_phase(Side::A, TransferPhase::Released, 1);
    harness.node_a.begin_shutdown();
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 3)).await;
    let released = adverts(&harness.net, Side::A, TransferPhase::Released);
    let (first, first_packet) = released.first().context("first Released send")?;
    let (second, second_packet) = released.get(1).context("next periodic Released send")?;
    assert_eq!(first_packet.transfer_id, second_packet.transfer_id);
    assert_eq!(
        second.at - first.at,
        Duration::from_millis(ADVERT_INTERVAL_MS)
    );
    assert!(harness.node_b.snapshot().owned);
    assert_advert_order(&harness.net);
    print_transfer_sequence("lost release", &harness.net);
    harness.stop().await
}

async fn returning_rank_winner(policy: FailurePolicy, preempt: bool) -> Harness {
    let mut config_a = node_config(Side::A, policy.clone());
    config_a.priority = 90;
    let mut harness = Harness::boot_configs(config_a, node_config(Side::B, policy.clone()));
    settle(BOOT_WINDOW).await;
    assert_single_owner("incumbent", &harness.node_b, &harness.node_a);
    harness.restart(Side::A, policy, preempt).await;
    harness
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_preempt_requests_only_after_stable_hold_down() -> Result<()> {
    let harness = returning_rank_winner(FailurePolicy::Available, true).await;
    // Gate the incumbent's withdrawal: rank and Requesting must never cause
    // the returning node to touch the resource before Released.
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    harness
        .node_b
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gate_next_del = Some(CommandGate {
        entered: entered_tx,
        release: release_rx,
    });
    entered_rx
        .await
        .context("preemption must request the incumbent's withdrawal")?;
    let requesting = adverts(&harness.net, Side::A, TransferPhase::Requesting);
    let (request, _) = requesting
        .first()
        .context("returning rank winner must request")?;
    let request_packet = decode_advert(&request.bytes).context("valid request")?;
    let first_fresh_owner = harness
        .net
        .history()
        .into_iter()
        .find(|sent| {
            sent.side == Side::B
                && decode_advert(&sent.bytes).is_some_and(|packet| {
                    packet.receiver_nonce == request_packet.sender_nonce
                        && packet.authority_source
                            != super::super::protocol::AdvertAuthoritySource::None
                })
        })
        .context("fresh incumbent advert after restart")?;
    assert!(request.at - first_fresh_owner.at >= Duration::from_millis(HOLD_DOWN_MS));
    assert_eq!(kernel_add_calls(&harness.node_a), Vec::<Vec<String>>::new());
    assert!(adverts(&harness.net, Side::B, TransferPhase::Released).is_empty());
    let _ = release_tx.send(Ok(()));
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 3)).await;
    assert_single_owner("preempt complete", &harness.node_a, &harness.node_b);
    assert_advert_order(&harness.net);
    print_transfer_sequence("preempt true", &harness.net);
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_preempt_false_never_requests() -> Result<()> {
    let harness = returning_rank_winner(FailurePolicy::Available, false).await;
    settle(BOOT_WINDOW).await;
    assert_single_owner("preempt disabled", &harness.node_b, &harness.node_a);
    assert!(adverts(&harness.net, Side::A, TransferPhase::Requesting).is_empty());
    assert_eq!(kernel_add_calls(&harness.node_a), Vec::<Vec<String>>::new());
    harness.stop().await
}

async fn run_lost_owned(policy: FailurePolicy) -> Result<()> {
    let mut config_a = node_config(Side::A, policy.clone());
    config_a.withdraw_timeout_ms = 60_000;
    let mut harness = Harness::boot_configs(config_a, node_config(Side::B, policy));
    settle(BOOT_WINDOW).await;
    harness
        .net
        .set_drop_phase(Side::B, TransferPhase::Owned, usize::MAX);
    // Also lose Returned: consent will eventually lapse at the successor,
    // but the origin cannot infer a return from silence.
    harness.node_a.begin_shutdown();
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 2)).await;
    let release = adverts(&harness.net, Side::A, TransferPhase::Released);
    assert!(!release.is_empty());
    let adds = kernel_add_calls(&harness.node_a).len();
    harness.net.set_drop(Side::B, DropRule::All);
    settle(dead_window() * 10).await;
    assert_eq!(
        kernel_add_calls(&harness.node_a).len(),
        adds,
        "I8: released owner reacquired without Returned"
    );
    assert!(!harness.node_a.snapshot().owned);
    assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
    assert!(
        !harness.node_a.task.is_finished(),
        "origin must remain alive throughout I8 check"
    );
    assert_advert_order(&harness.net);
    print_transfer_sequence("lost owned, no return", &harness.net);
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_strict_released_owner_never_reacquires_on_lost_owned() -> Result<()> {
    run_lost_owned(FailurePolicy::Strict).await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_available_released_owner_blocks_assumed_fallback() -> Result<()> {
    let harness = returning_rank_winner(FailurePolicy::Available, true).await;
    harness
        .net
        .set_drop_phase(Side::A, TransferPhase::Owned, usize::MAX);
    settle(BOOT_WINDOW).await;
    assert!(!adverts(&harness.net, Side::B, TransferPhase::Released).is_empty());
    let adds = kernel_add_calls(&harness.node_b).len();
    harness.net.set_drop(Side::A, DropRule::All);
    settle(dead_window() * 10).await;
    assert_eq!(
        kernel_add_calls(&harness.node_b).len(),
        adds,
        "I8: timeout cannot undo release"
    );
    assert!(!harness.node_b.snapshot().owned);
    assert_eq!(harness.node_b.kernel_table(), Vec::<String>::new());
    assert!(harness.node_a.snapshot().owned);
    assert_advert_order(&harness.net);
    print_transfer_sequence(
        "available released owner, assumed fallback blocked",
        &harness.net,
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_failed_successor_returns_only_after_absence() -> Result<()> {
    let harness = returning_rank_winner(FailurePolicy::Available, true).await;
    harness
        .node_a
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .fail_add_addresses
        .push("10.0.0.10/24".to_owned());
    let old_term = harness.node_b.snapshot().authority.term;
    settle(BOOT_WINDOW).await;
    let returned = adverts(&harness.net, Side::A, TransferPhase::Returned);
    let (_, returned_packet) = returned.first().context("failed successor must return")?;
    let released = adverts(&harness.net, Side::B, TransferPhase::Released);
    assert_eq!(
        released.first().context("origin released")?.1.transfer_id,
        returned_packet.transfer_id
    );
    assert_single_owner("returned transfer", &harness.node_b, &harness.node_a);
    assert!(
        harness.node_b.snapshot().authority.term > old_term,
        "return must create a new term"
    );
    assert_advert_order(&harness.net);
    print_transfer_sequence("successor returned", &harness.net);
    harness.stop().await
}

/// An authority loss during a live add drains the worker before Returned.
/// The command gate deliberately completes an old add after revocation (I7).
#[tokio::test(start_paused = true)]
async fn matrix_handoff_cancelled_acquire_drains_before_return() -> Result<()> {
    let mut config_a = node_config(Side::A, FailurePolicy::Strict);
    config_a.withdraw_timeout_ms = 60_000;
    config_a.dead_factor = 2;
    let mut config_b = node_config(Side::B, FailurePolicy::Strict);
    config_b.dead_factor = 2;
    let mut harness = Harness::boot_configs(config_a, config_b);
    settle(BOOT_WINDOW).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    harness
        .node_b
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gate_after_add = Some(CommandGate {
        entered: entered_tx,
        release: release_rx,
    });
    harness.node_a.begin_shutdown();
    entered_rx
        .await
        .context("successor add must enter the gate")?;
    harness.net.set_drop(Side::A, DropRule::All);
    let remaining = harness
        .node_b
        .snapshot()
        .authority
        .expires_in_ms
        .context("strict acquisition must have a consent deadline")?;
    assert!(
        remaining + 1 < crate::gruezi::addresses::COMMAND_TIMEOUT_MS,
        "the consent deadline must precede the command gate timeout"
    );
    settle(Duration::from_millis(remaining + 1)).await;
    assert!(adverts(&harness.net, Side::B, TransferPhase::Owned).is_empty());
    assert!(adverts(&harness.net, Side::B, TransferPhase::Returned).is_empty());
    let _ = release_tx.send(Ok(()));
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 2)).await;
    assert_eq!(harness.node_b.kernel_table(), Vec::<String>::new());
    assert!(!adverts(&harness.net, Side::B, TransferPhase::Returned).is_empty());
    assert_advert_order(&harness.net);
    harness.stop().await
}

fn assert_no_two_kernel_owners(harness: &Harness, schedule: &str) {
    assert!(
        harness.node_a.kernel_table().is_empty() || harness.node_b.kernel_table().is_empty(),
        "I4: two kernel owners in {schedule}: A={:?}, B={:?}",
        harness.node_a.kernel_table(),
        harness.node_b.kernel_table()
    );
    for sent in harness.net.history() {
        assert!(
            sent.kernel_a.is_empty() || sent.kernel_b.is_empty(),
            "I4: two kernel owners at send time in {schedule}: {sent:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_origin_restart_with_lost_owned_waits_for_return() -> Result<()> {
    let mut config_a = node_config(Side::A, FailurePolicy::Strict);
    config_a.withdraw_timeout_ms = 60_000;
    let mut harness = Harness::boot_configs(config_a, node_config(Side::B, FailurePolicy::Strict));
    settle(BOOT_WINDOW).await;
    harness
        .net
        .set_drop_phase(Side::B, TransferPhase::Owned, usize::MAX);
    harness.node_a.begin_shutdown();
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 2)).await;
    assert!(harness.node_b.snapshot().owned);
    assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    harness
        .node_b
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gate_next_del = Some(CommandGate {
        entered: entered_tx,
        release: release_rx,
    });
    harness.restart(Side::A, FailurePolicy::Strict, false).await;
    entered_rx
        .await
        .context("new session must revoke the successor's previous grant")?;
    assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
    assert_ne!(harness.node_b.kernel_table(), Vec::<String>::new());
    assert_eq!(
        kernel_add_calls(&harness.node_a).len(),
        1,
        "restarted origin cannot reacquire while successor is draining"
    );
    assert!(adverts(&harness.net, Side::B, TransferPhase::Returned).is_empty());
    println!(
        "I8 RESTART: every Owned advert dropped; restarted A adds=0; exactly one kernel owner B; A={:?} B={:?}",
        harness.node_a.kernel_table(),
        harness.node_b.kernel_table()
    );
    let _ = release_tx.send(Ok(()));
    settle(BOOT_WINDOW).await;
    assert_no_two_kernel_owners(&harness, "lost Owned/origin restart/return");
    assert_advert_order(&harness.net);
    assert!(!adverts(&harness.net, Side::B, TransferPhase::Returned).is_empty());
    // Strict grants never survive a peer restart. Once B explicitly returns
    // after absence, the new session may bootstrap A under the rank rule.
    assert_single_owner(
        "restart after explicit return",
        &harness.node_a,
        &harness.node_b,
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_handoff_new_session_cancels_inflight_transfer_grant() -> Result<()> {
    let mut config_a = node_config(Side::A, FailurePolicy::Strict);
    config_a.withdraw_timeout_ms = 60_000;
    let mut harness = Harness::boot_configs(config_a, node_config(Side::B, FailurePolicy::Strict));
    settle(BOOT_WINDOW).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    harness
        .node_b
        .kernel
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .gate_after_add = Some(CommandGate {
        entered: entered_tx,
        release: release_rx,
    });
    harness.node_a.begin_shutdown();
    entered_rx.await.context("transfer add entered")?;
    harness.restart(Side::A, FailurePolicy::Strict, false).await;
    // Re-establishment takes two intervals, within the three-second command
    // bound. The new nonce must revoke the old transfer grant while gated.
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 2)).await;
    assert_eq!(harness.node_a.kernel_table(), Vec::<String>::new());
    assert!(adverts(&harness.net, Side::B, TransferPhase::Owned).is_empty());
    let _ = release_tx.send(Ok(()));
    settle(BOOT_WINDOW).await;
    assert!(!adverts(&harness.net, Side::B, TransferPhase::Returned).is_empty());
    assert_no_two_kernel_owners(&harness, "origin restart/inflight transfer");
    assert_advert_order(&harness.net);
    harness.stop().await
}

/// Exhaustive fixed two-step alphabet: loss (including mid-flight), duplicate,
/// reorder, crash and process restart, applied around a real strict handoff.
#[tokio::test(start_paused = true)]
async fn matrix_handoff_i4_bounded_schedule_enumeration() -> Result<()> {
    let events = [
        "loss-a",
        "loss-b",
        "duplicate",
        "reorder",
        "crash-a",
        "restart-a",
        "restart-b",
    ];
    let mut explored = 0_u64;
    for first in events {
        for second in events {
            let mut config_a = node_config(Side::A, FailurePolicy::Strict);
            config_a.withdraw_timeout_ms = 60_000;
            let mut harness =
                Harness::boot_configs(config_a, node_config(Side::B, FailurePolicy::Strict));
            settle(BOOT_WINDOW).await;
            harness.node_a.begin_shutdown();
            let schedule = format!("{first}/{second}");
            for event in [first, second] {
                match event {
                    "loss-a" => harness.net.set_drop(Side::A, DropRule::All),
                    "loss-b" => harness.net.set_drop(Side::B, DropRule::All),
                    "duplicate" => {
                        harness.net.set_duplicate(Side::A, 2);
                        harness.net.set_duplicate(Side::B, 2);
                    }
                    "reorder" => {
                        harness.net.set_reorder(Side::A, 1);
                        harness.net.set_reorder(Side::B, 1);
                    }
                    "crash-a" => harness.node_a.crash(),
                    "restart-a" => harness.restart(Side::A, FailurePolicy::Strict, false).await,
                    "restart-b" => harness.restart(Side::B, FailurePolicy::Strict, false).await,
                    _ => return Err(anyhow!("unknown deterministic schedule event: {event}")),
                }
                // Sample on both sides of every advert and consent boundary.
                for _ in 0..16 {
                    settle(Duration::from_millis(250)).await;
                    assert_no_two_kernel_owners(&harness, &schedule);
                }
            }
            harness.net.set_drop(Side::A, DropRule::None);
            harness.net.set_drop(Side::B, DropRule::None);
            for _ in 0..20 {
                settle(Duration::from_millis(250)).await;
                assert_no_two_kernel_owners(&harness, &schedule);
            }
            assert_advert_order(&harness.net);
            harness.stop().await?;
            explored += 1;
        }
    }
    println!(
        "I4 ENUMERATION: explored_schedules={explored}; two-owner_states=0; alphabet=loss/duplication/reordering/crash/restart; kernel snapshots at every send and 250ms fake-time boundary"
    );
    Ok(())
}

/// Captures tracing output written by tasks on this test's thread.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    fn contents(&self) -> String {
        let guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&guard).into_owned()
    }
}

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
