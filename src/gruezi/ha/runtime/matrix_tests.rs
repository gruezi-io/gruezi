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
    in_memory::{DropRule, InMemoryEndpoint, InMemoryNetwork, Side},
    run_with_transport,
};
use crate::{
    config::{FailurePolicy, WatchdogConfig},
    gruezi::{
        addresses::{CommandRunner, SimulatedKernel},
        ha::{
            HaAuth, HaDecisionReason, HaRuntimeConfig, HaStatus,
            status::{SafetyLevel, TransitionTrigger},
        },
        hooks::HaHooks,
        ownership::AuthoritySource,
    },
};
use anyhow::{Result, anyhow};
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

    async fn stop(mut self) -> Result<()> {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
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
        let (net, endpoint_a, endpoint_b) = InMemoryNetwork::new_pair();
        let node_a = spawn_node(Side::A, endpoint_a, policy.clone());
        let node_b = spawn_node(Side::B, endpoint_b, policy);
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
}

fn spawn_node(side: Side, endpoint: InMemoryEndpoint, policy: FailurePolicy) -> Node {
    let runtime = node_config(side, policy);
    let name = match side {
        Side::A => "node-a",
        Side::B => "node-b",
    };
    let kernel = SimulatedKernel::new_shared();
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
