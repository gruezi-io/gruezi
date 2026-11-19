//! Lifecycle rows (plan task 5, B1 and B2 core) on the real coordinator.
//!
//! Both nodes run the real select loop, the real resource worker over the
//! simulated kernel, and the real lifecycle executor queue. Only the script
//! body is scripted (`hooks::scripted`), so every row runs under
//! `tokio::time::pause` with exact virtual timestamps. The executor's queue,
//! coalescing, cancellation and generation rules are the production ones.
//!
//! Each row prints the virtual-time evidence it asserts: the scripted calls
//! (operation, generation, binding, start/end, outcome), the status timeline
//! and the adverts, so a green run shows ordering instead of only a result.

use super::*;
use crate::{
    config::{AuthorityConfig, LifecycleConfig, WatchdogMode},
    gruezi::{
        ha::{
            protocol::{AdvertAuthoritySource, AdvertResourceState},
            runtime::authority_commands::{
                AuthorityCommandRunner, ScriptedAuthority, ScriptedOutcome,
            },
        },
        hooks::{
            ActivationMode, DeactivationMode, HookEvent, LifecycleKind, LifecycleOperation,
            LifecycleOutcome,
            scripted::{ScriptedCall, ScriptedLifecycle, Step},
        },
        watchdog::{FakeWatchdog, WatchdogCall, WatchdogFault},
    },
};
use tokio::time::Instant;

/// Short rows: `P = V = D = 1 s`, detection bound `V + D = 2 s`.
const SHORT_MS: u64 = 1_000;
/// Hanging-script rows: `D = 4 s` exceeds the 3 s consent window, so a
/// revocation observed before `D` was caused by authority, not by the script.
const LONG_MS: u64 = 4_000;
/// Two termination grace periods (`CANCEL_GRACE_MS`): SIGTERM-to-SIGKILL plus
/// the bounded pipe drain the executor may spend after a deadline.
const TERMINATION_MS: u64 = 2 * crate::gruezi::hooks::CANCEL_GRACE_MS;

type Script = Arc<Mutex<ScriptedLifecycle>>;
type Authority = Arc<Mutex<ScriptedAuthority>>;

struct LifecycleNode {
    node: Node,
    script: Script,
}

struct LifecyclePair {
    net: InMemoryNetwork,
    a: LifecycleNode,
    b: LifecycleNode,
}

impl LifecyclePair {
    async fn stop(self) -> Result<()> {
        self.a.node.stop().await?;
        self.b.node.stop().await
    }
}

#[derive(Clone)]
struct NodeSeams {
    authority: Option<Authority>,
    watchdog: Option<FakeWatchdog>,
}

impl NodeSeams {
    const fn none() -> Self {
        Self {
            authority: None,
            watchdog: None,
        }
    }
}

fn lifecycle_config(side: Side, policy: FailurePolicy, timeout_ms: u64) -> HaRuntimeConfig {
    let mut config = node_config(side, policy);
    config.command_timeout_ms = config.command_timeout_ms.max(timeout_ms);
    config.lifecycle = Some(LifecycleConfig {
        activate_command: "/usr/lib/gruezi/lifecycle-activate".to_owned(),
        deactivate_command: "/usr/lib/gruezi/lifecycle-deactivate".to_owned(),
        timeout_ms,
    });
    config
}

fn with_hooks(mut config: HaRuntimeConfig) -> HaRuntimeConfig {
    config.hooks = HaHooks {
        on_promote: Some("/usr/lib/gruezi/on-promote".to_owned()),
        on_demote: Some("/usr/lib/gruezi/on-demote".to_owned()),
        on_backup: Some("/usr/lib/gruezi/on-backup".to_owned()),
        on_fault: Some("/usr/lib/gruezi/on-fault".to_owned()),
        ..HaHooks::default()
    };
    config
}

fn external(mut config: HaRuntimeConfig) -> HaRuntimeConfig {
    config.failure_policy = FailurePolicy::External;
    config.authority = Some(AuthorityConfig {
        check_command: "authority-check".to_owned(),
        fence_command: None,
        interval_ms: Some(ADVERT_INTERVAL_MS),
        grant_ttl_ms: 20_000,
    });
    config
}

fn scripted_authority(grant: bool) -> Authority {
    Arc::new(Mutex::new(ScriptedAuthority {
        check: ScriptedOutcome::Exit(grant),
        ..ScriptedAuthority::default()
    }))
}

fn set_grant(authority: &Authority, grant: bool) {
    authority
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .check = ScriptedOutcome::Exit(grant);
}

fn spawn_lifecycle_node(
    side: Side,
    endpoint: InMemoryEndpoint,
    mut runtime: HaRuntimeConfig,
    kernel: Arc<Mutex<SimulatedKernel>>,
    seams: NodeSeams,
    script: Script,
) -> LifecycleNode {
    let watchdog = seams.watchdog.map_or_else(WatchdogDevice::default, |fake| {
        runtime.watchdog.mode = WatchdogMode::Required;
        // P + Q + B < W with Q = 3 * COMMAND_TIMEOUT_MS for one address.
        runtime.watchdog.timeout_ms = 30_000;
        WatchdogDevice::Fake(fake)
    });
    let node = super::spawn_with_seams(
        side,
        endpoint,
        runtime,
        super::super::Seams {
            commands: CommandRunner::Simulated(kernel.clone()),
            authority: seams.authority.map_or(
                AuthorityCommandRunner::System,
                AuthorityCommandRunner::Scripted,
            ),
            watchdog,
            lifecycle: super::super::LifecycleRunner::Scripted(script.clone()),
        },
        kernel,
    );
    LifecycleNode { node, script }
}

fn boot_pair(
    config_a: HaRuntimeConfig,
    config_b: HaRuntimeConfig,
    seams_a: NodeSeams,
    seams_b: NodeSeams,
) -> LifecyclePair {
    let (net, endpoint_a, endpoint_b) = InMemoryNetwork::new_pair();
    let a = spawn_lifecycle_node(
        Side::A,
        endpoint_a,
        config_a,
        SimulatedKernel::new_shared(),
        seams_a,
        ScriptedLifecycle::shared(),
    );
    let b = spawn_lifecycle_node(
        Side::B,
        endpoint_b,
        config_b,
        SimulatedKernel::new_shared(),
        seams_b,
        ScriptedLifecycle::shared(),
    );
    net.capture_kernels(a.node.kernel.clone(), b.node.kernel.clone());
    LifecyclePair { net, a, b }
}

/// External policy pair: A's grant follows `grant`; B never holds one.
fn external_pair(timeout_ms: u64, grant: &Authority, hooks: bool) -> LifecyclePair {
    let config = |side| {
        let config = external(lifecycle_config(side, FailurePolicy::External, timeout_ms));
        if hooks { with_hooks(config) } else { config }
    };
    boot_pair(
        config(Side::A),
        config(Side::B),
        NodeSeams {
            authority: Some(grant.clone()),
            watchdog: None,
        },
        NodeSeams {
            authority: Some(scripted_authority(false)),
            watchdog: None,
        },
    )
}

fn boot_strict(timeout_ms: u64) -> LifecyclePair {
    boot_pair(
        lifecycle_config(Side::A, FailurePolicy::Strict, timeout_ms),
        lifecycle_config(Side::B, FailurePolicy::Strict, timeout_ms),
        NodeSeams::none(),
        NodeSeams::none(),
    )
}

/// One `available` node whose peer never runs: the lone node promotes on its
/// startup deadline, and a shutdown never finds a peer for a handoff.
fn boot_single(timeout_ms: u64, seams: NodeSeams) -> (InMemoryNetwork, LifecycleNode) {
    let (net, endpoint, _peer) = InMemoryNetwork::new_pair();
    let node = spawn_lifecycle_node(
        Side::A,
        endpoint,
        with_hooks(lifecycle_config(
            Side::A,
            FailurePolicy::Available,
            timeout_ms,
        )),
        SimulatedKernel::new_shared(),
        seams,
        ScriptedLifecycle::shared(),
    );
    (net, node)
}

fn script_set(script: &Script, update: impl FnOnce(&mut ScriptedLifecycle)) {
    update(&mut script.lock().unwrap_or_else(PoisonError::into_inner));
}

fn calls(script: &Script) -> Vec<ScriptedCall> {
    script
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .calls
        .clone()
}

fn calls_of(script: &Script, kind: LifecycleKind) -> Vec<ScriptedCall> {
    calls(script)
        .into_iter()
        .filter(|call| kind_of(call.operation) == kind)
        .collect()
}

const fn kind_of(operation: LifecycleOperation) -> LifecycleKind {
    match operation {
        LifecycleOperation::Activate(_) => LifecycleKind::Activate,
        LifecycleOperation::Deactivate(_) => LifecycleKind::Deactivate,
        LifecycleOperation::Verify => LifecycleKind::Verify,
        LifecycleOperation::Notify(event) => LifecycleKind::Notify(event),
    }
}

fn hex(nonce: [u8; 16]) -> String {
    format!("{:032x}", u128::from_be_bytes(nonce))
}

fn ms(origin: Instant, at: Instant) -> u128 {
    at.saturating_duration_since(origin).as_millis()
}

fn print_calls(row: &str, node: &LifecycleNode, origin: Instant) {
    print_script(row, node.node.name, &node.script, origin);
}

fn print_script(row: &str, name: &str, script: &Script, origin: Instant) {
    for call in calls(script) {
        println!(
            "LIFECYCLE[{row}] {name} t={}ms..{} op={:?} generation={} transfer_id={:?} peer_nonce={:?} deadline=+{}ms outcome={:?}",
            ms(origin, call.started),
            call.finished.map_or_else(
                || "running".to_owned(),
                |end| format!("{}ms", ms(origin, end))
            ),
            call.operation,
            call.generation,
            call.transfer_id,
            call.peer_nonce.map(hex),
            ms(call.started, call.deadline),
            call.outcome,
        );
    }
}

fn print_status(row: &str, phase: &str, node: &LifecycleNode) {
    print_matrix_state(row, phase, &node.node);
    let status = node.node.snapshot();
    println!(
        "LIFECYCLE[{row}] {phase} node={} lifecycle={} generation={} activation_confirmed={} passive_confirmed={} verification={} operation={:?} last_failure={:?}",
        node.node.name,
        status.lifecycle.phase,
        status.lifecycle.generation,
        status.lifecycle.confirmations.activation_confirmed,
        status.lifecycle.confirmations.passive_confirmed,
        status.lifecycle.verification.state,
        status.lifecycle.operation,
        status.lifecycle.last_critical_failure,
    );
}

/// The executor runs one job at a time: no two scripted jobs overlap, ever.
fn assert_serial(row: &str, script: &Script) {
    let mut calls = calls(script);
    calls.sort_by_key(|call| call.started);
    for pair in calls.windows(2) {
        if let [earlier, later] = pair {
            let end = earlier
                .finished
                .unwrap_or(later.started + Duration::from_secs(1));
            assert!(
                end <= later.started,
                "{row}: overlapping scripts {earlier:?} and {later:?}"
            );
        }
    }
}

fn side_adverts(net: &InMemoryNetwork, side: Side) -> Vec<(SentAdvert, HaPacket)> {
    net.history()
        .into_iter()
        .filter_map(|sent| {
            let packet = decode_advert(&sent.bytes)?;
            (sent.side == side).then_some((sent, packet))
        })
        .collect()
}

fn add_calls(node: &LifecycleNode) -> Vec<Vec<String>> {
    kernel_add_calls(&node.node)
}

fn announce_calls(node: &LifecycleNode) -> Vec<Vec<String>> {
    node.node
        .kernel_calls()
        .into_iter()
        .filter(|call| call.first().is_some_and(|command| command == "arping"))
        .collect()
}

/// Advance in steps until `ready` holds, bounded by `limit`.
async fn settle_until(
    limit: Duration,
    step: Duration,
    mut ready: impl FnMut() -> bool,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    while !ready() {
        if Instant::now() >= deadline {
            return Err(anyhow!("condition not reached within {limit:?}"));
        }
        settle(step).await;
    }
    Ok(())
}

async fn wait_owned(node: &LifecycleNode) -> Result<()> {
    settle_until(BOOT_WINDOW * 2, Duration::from_millis(100), || {
        node.node.snapshot().owned
    })
    .await
    .context("lifecycle node never owned")
}

/// Wait until a job of `kind` (after `skip` earlier ones) has started.
async fn wait_started(
    node: &LifecycleNode,
    kind: LifecycleKind,
    skip: usize,
) -> Result<ScriptedCall> {
    settle_until(BOOT_WINDOW * 2, Duration::from_millis(10), || {
        calls_of(&node.script, kind).len() > skip
    })
    .await
    .with_context(|| format!("{kind:?} job never started"))?;
    calls_of(&node.script, kind)
        .get(skip)
        .cloned()
        .context("started job")
}

async fn wait_finished(
    node: &LifecycleNode,
    kind: LifecycleKind,
    skip: usize,
) -> Result<ScriptedCall> {
    settle_until(BOOT_WINDOW * 2, Duration::from_millis(10), || {
        calls_of(&node.script, kind)
            .get(skip)
            .is_some_and(|call| call.finished.is_some())
    })
    .await
    .with_context(|| format!("{kind:?} job never finished"))?;
    calls_of(&node.script, kind)
        .get(skip)
        .cloned()
        .context("finished job")
}

/// The first job of `kind` started at or after `since`, once it finished.
async fn wait_finished_since(
    node: &LifecycleNode,
    kind: LifecycleKind,
    since: Instant,
) -> Result<ScriptedCall> {
    let find = || {
        calls_of(&node.script, kind)
            .into_iter()
            .find(|call| call.started >= since)
    };
    settle_until(BOOT_WINDOW * 2, Duration::from_millis(10), || {
        find().is_some_and(|call| call.finished.is_some())
    })
    .await
    .with_context(|| format!("{kind:?} job after {since:?} never finished"))?;
    find().context("finished job")
}

/// Records every published status the watcher observes, in virtual time.
fn record_timeline(node: &Node, origin: Instant) -> Arc<Mutex<Vec<(u128, String)>>> {
    let timeline = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&timeline);
    let mut status_rx = node.status_rx.clone();
    tokio::spawn(async move {
        let mut last = String::new();
        loop {
            let entry = {
                let status = status_rx.borrow_and_update();
                format!(
                    "authority={} source={:?} lifecycle={} resource={} owned={}",
                    status.authority.state,
                    status.authority.source,
                    status.lifecycle.phase,
                    status.resource.state,
                    status.owned
                )
            };
            if entry != last {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((ms(origin, Instant::now()), entry.clone()));
                last = entry;
            }
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    });
    timeline
}

fn first_index(timeline: &[(u128, String)], needle: &str) -> Option<usize> {
    timeline
        .iter()
        .position(|(_, entry)| entry.contains(needle))
}

// ---------------------------------------------------------------- task 5 rows

#[tokio::test(start_paused = true)]
async fn delayed_activation_blocks_vip_announce_and_owned() -> Result<()> {
    let row = "delayed_activation";
    let origin = Instant::now();
    let pair = boot_strict(SHORT_MS);
    script_set(&pair.a.script, |script| {
        script.activate = Step::Delay(Duration::from_millis(800), 0);
    });
    let activate = wait_started(&pair.a, LifecycleKind::Activate, 0).await?;
    settle(Duration::from_millis(400)).await;
    let during = pair.a.node.snapshot();
    print_status(row, "activation running", &pair.a);
    assert_eq!(
        during.authority.state, "valid",
        "consent precedes activation"
    );
    assert_eq!(during.lifecycle.phase, "activating");
    assert!(!during.owned);
    assert!(add_calls(&pair.a).is_empty(), "no VIP add while activating");
    assert!(
        announce_calls(&pair.a).is_empty(),
        "no announcement while activating"
    );
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    let finished = wait_finished(&pair.a, LifecycleKind::Activate, 0).await?;
    wait_owned(&pair.a).await?;
    assert_eq!(finished.outcome, Some(LifecycleOutcome::Succeeded));
    for (sent, packet) in side_adverts(&pair.net, Side::A) {
        if sent.at < finished.finished.unwrap_or(sent.at) {
            assert_ne!(packet.resource_state, AdvertResourceState::Present);
            assert!(sent.kernel_a.is_empty(), "VIP before activation: {sent:?}");
        }
    }
    assert_eq!(add_calls(&pair.a).len(), 1);
    println!(
        "LIFECYCLE[{row}] RESULT activation {}ms..{}ms; adds before completion=0 announces before completion=0 owned_during=false owned_after=true",
        ms(origin, activate.started),
        finished.finished.map_or(0, |end| ms(origin, end)),
    );
    print_calls(row, &pair.a, origin);
    pair.stop().await
}

/// QA timeline: consent -> Activating -> Ready -> VIP add -> announce -> owned.
#[tokio::test(start_paused = true)]
async fn activation_success_permits_vip_in_order() -> Result<()> {
    let row = "activation_order";
    let origin = Instant::now();
    let pair = boot_strict(SHORT_MS);
    script_set(&pair.a.script, |script| {
        script.activate = Step::Delay(Duration::from_millis(300), 0);
    });
    let timeline = record_timeline(&pair.a.node, origin);
    wait_owned(&pair.a).await?;
    settle(Duration::from_millis(100)).await;
    let timeline = timeline
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for (at, entry) in &timeline {
        println!("TIMELINE[{row}] node-a t={at}ms {entry}");
    }
    let activate = calls_of(&pair.a.script, LifecycleKind::Activate)
        .first()
        .cloned()
        .context("activation call")?;
    let activated_at = activate.finished.context("activation finished")?;
    println!(
        "TIMELINE[{row}] node-a t={}ms script activate start mode={:?} generation={}",
        ms(origin, activate.started),
        activate.operation,
        activate.generation
    );
    println!(
        "TIMELINE[{row}] node-a t={}ms script activate exit=0 (Ready)",
        ms(origin, activated_at)
    );
    let kernel_calls = pair.a.node.kernel_calls();
    let add = kernel_calls
        .iter()
        .position(|call| call.iter().any(|part| part == "add"))
        .context("VIP add")?;
    let announce = kernel_calls
        .iter()
        .position(|call| call.first().is_some_and(|command| command == "arping"))
        .context("VIP announcement")?;
    println!("TIMELINE[{row}] node-a kernel calls in order: {kernel_calls:?}");
    let consent = first_index(&timeline, "authority=valid").context("consent")?;
    let activating = first_index(&timeline, "lifecycle=activating").context("activating")?;
    let ready = first_index(&timeline, "lifecycle=ready").context("ready")?;
    let present = first_index(&timeline, "resource=present").context("present")?;
    let owned = first_index(&timeline, "owned=true").context("owned")?;
    assert!(consent <= activating, "consent before Activating");
    assert!(activating < ready, "Activating before Ready");
    assert!(ready <= present, "Ready before the VIP is present");
    assert!(present <= owned, "VIP confirmed before owned");
    assert!(add < announce, "add before announcement");
    let present_at = timeline
        .get(present)
        .map(|(at, _)| *at)
        .context("present time")?;
    assert!(
        ms(origin, activated_at) <= present_at,
        "the VIP appears only after the activation succeeded"
    );
    for (sent, packet) in side_adverts(&pair.net, Side::A) {
        if packet.resource_state == AdvertResourceState::Present {
            assert!(sent.at >= activated_at, "Present advert before Ready");
        }
    }
    assert_eq!(
        activate.operation,
        LifecycleOperation::Activate(ActivationMode::Cold),
        "an initial formation is a cold bootstrap (errata F-02)"
    );
    println!(
        "LIFECYCLE[{row}] RESULT order consent<=activating<ready<=vip_add<announce, present<=owned=true"
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn failed_timed_out_or_missing_activate_compensates() -> Result<()> {
    for (step, outcome, status_outcome) in [
        (Step::Exit(1), LifecycleOutcome::Failed, "failed"),
        (Step::Hang, LifecycleOutcome::TimedOut, "timed_out"),
        (
            Step::SpawnError,
            LifecycleOutcome::SpawnError,
            "spawn_error",
        ),
    ] {
        let row = format!("activate_{status_outcome}");
        let origin = Instant::now();
        let pair = boot_strict(SHORT_MS);
        script_set(&pair.a.script, |script| script.activate = step);
        let activate = wait_finished(&pair.a, LifecycleKind::Activate, 0).await?;
        assert_eq!(activate.outcome, Some(outcome));
        // Baseline deactivate is #0; the compensation is the next one.
        let compensation = wait_finished(&pair.a, LifecycleKind::Deactivate, 1).await?;
        settle(Duration::from_millis(50)).await;
        print_status(&row, "after compensation", &pair.a);
        let status = pair.a.node.snapshot();
        assert!(!status.owned);
        assert!(add_calls(&pair.a).is_empty(), "{row}: no VIP add");
        assert!(compensation.generation > activate.generation);
        assert_eq!(
            compensation.operation,
            LifecycleOperation::Deactivate(DeactivationMode::Revoke)
        );
        assert!(compensation.started >= activate.finished.context("activate end")?);
        let failure = status
            .lifecycle
            .last_critical_failure
            .context("retained failure")?;
        assert_eq!(failure.event, "activate");
        assert_eq!(failure.outcome, status_outcome);
        if step == Step::Hang {
            assert_eq!(
                ms(activate.started, activate.finished.context("end")?),
                u128::from(SHORT_MS + TERMINATION_MS / 2),
                "timed out at P plus the termination grace"
            );
        }
        // A fixed script recovers through the ordinary path, paced by P.
        script_set(&pair.a.script, |script| script.activate = Step::Exit(0));
        wait_owned(&pair.a).await?;
        print_calls(&row, &pair.a, origin);
        println!(
            "LIFECYCLE[{row}] RESULT outcome={outcome:?} adds_during_failure=0 compensation=deactivate(revoke) g{} release_withheld_until_inactive recovered_owner=true",
            compensation.generation
        );
        pair.stop().await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn promote_demote_promote_never_overlap_scripts() -> Result<()> {
    let row = "promote_demote_promote";
    let origin = Instant::now();
    let grant = scripted_authority(true);
    let pair = external_pair(SHORT_MS, &grant, true);
    script_set(&pair.a.script, |script| {
        script.activate = Step::Delay(Duration::from_millis(300), 0);
        script.deactivate = Step::Delay(Duration::from_millis(300), 0);
        script.verify = Step::Delay(Duration::from_millis(100), 0);
        script.notify = Step::Delay(Duration::from_millis(200), 0);
    });
    wait_owned(&pair.a).await?;
    set_grant(&grant, false);
    settle_until(BOOT_WINDOW, Duration::from_millis(50), || {
        pair.a.node.snapshot().lifecycle.phase == "inactive"
    })
    .await?;
    assert!(!pair.a.node.snapshot().owned);
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    set_grant(&grant, true);
    settle(Duration::from_millis(200)).await;
    wait_owned(&pair.a).await?;
    settle(Duration::from_secs(3)).await;
    print_calls(row, &pair.a, origin);
    assert_serial(row, &pair.a.script);
    let critical: Vec<_> = calls(&pair.a.script)
        .into_iter()
        .filter(|call| !matches!(call.operation, LifecycleOperation::Notify(_)))
        .map(|call| (call.operation, call.outcome))
        .collect();
    let activations = critical
        .iter()
        .filter(|(operation, outcome)| {
            matches!(operation, LifecycleOperation::Activate(_))
                && *outcome == Some(LifecycleOutcome::Succeeded)
        })
        .count();
    assert_eq!(activations, 2, "promote, demote, promote: {critical:?}");
    assert!(
        calls(&pair.a.script)
            .iter()
            .any(|call| call.operation == LifecycleOperation::Notify(HookEvent::Promote)),
        "on_promote is a serialized executor job"
    );
    println!(
        "LIFECYCLE[{row}] RESULT {} jobs, zero overlap; activations={activations}",
        calls(&pair.a.script).len()
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn stale_success_cannot_restore_ownership_or_clear_newer_failure() -> Result<()> {
    let row = "stale_success";
    let origin = Instant::now();
    let grant = scripted_authority(true);
    let pair = external_pair(LONG_MS, &grant, false);
    script_set(&pair.a.script, |script| {
        // Would succeed at +3 s; the grant ends first.
        script.activate = Step::Delay(Duration::from_millis(3_000), 0);
    });
    let activate = wait_started(&pair.a, LifecycleKind::Activate, 0).await?;
    // Only the compensation fails; the startup baseline already succeeded.
    script_set(&pair.a.script, |script| script.deactivate = Step::Exit(1));
    set_grant(&grant, false);
    settle(Duration::from_secs(5)).await;
    let activate = calls_of(&pair.a.script, LifecycleKind::Activate)
        .into_iter()
        .find(|call| call.generation == activate.generation)
        .context("activation record")?;
    assert_eq!(activate.outcome, Some(LifecycleOutcome::Cancelled));
    print_status(row, "after newer deactivate failure", &pair.a);
    let status = pair.a.node.snapshot();
    assert!(!status.owned);
    assert_eq!(status.lifecycle.phase, "uncertain");
    let failure = status
        .lifecycle
        .last_critical_failure
        .context("newer failure")?;
    assert_eq!(failure.event, "deactivate");
    assert!(failure.generation > activate.generation);
    // A renewed grant cannot reacquire before restart, nor clear the record.
    set_grant(&grant, true);
    settle(Duration::from_secs(5)).await;
    let status = pair.a.node.snapshot();
    assert!(!status.owned);
    assert_eq!(add_calls(&pair.a), Vec::<Vec<String>>::new());
    assert_eq!(
        status
            .lifecycle
            .last_critical_failure
            .map(|failure| failure.event),
        Some("deactivate".to_owned())
    );
    let uncertain = side_adverts(&pair.net, Side::A)
        .last()
        .map(|(_, packet)| packet.resource_state);
    assert_eq!(uncertain, Some(AdvertResourceState::Uncertain));
    print_calls(row, &pair.a, origin);
    println!(
        "LIFECYCLE[{row}] RESULT stale activation g{} reported Cancelled; newer deactivate failure retained; owned=false advert=uncertain",
        activate.generation
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn vip_absent_with_pending_deactivate_blocks_bootstrap_release_and_disarm() -> Result<()> {
    let row = "pending_deactivate";
    let origin = Instant::now();
    let watchdog_a = FakeWatchdog::new(WatchdogFault::None);
    let pair = boot_pair(
        lifecycle_config(Side::A, FailurePolicy::Strict, LONG_MS),
        lifecycle_config(Side::B, FailurePolicy::Strict, LONG_MS),
        NodeSeams {
            authority: None,
            watchdog: Some(watchdog_a.clone()),
        },
        NodeSeams {
            authority: None,
            watchdog: Some(FakeWatchdog::new(WatchdogFault::None)),
        },
    );
    wait_owned(&pair.a).await?;
    assert!(pair.a.node.snapshot().watchdog.armed);
    script_set(&pair.a.script, |script| {
        script.deactivate = Step::Delay(Duration::from_millis(2_500), 0);
    });
    // Leave the instant in which ownership completed, so every advert at or
    // after `shutdown_at` was sent by an owner that has begun to release.
    settle(Duration::from_millis(100)).await;
    let mut a = pair.a;
    let shutdown_at = Instant::now();
    a.node.begin_shutdown();
    settle(Duration::from_millis(1_000)).await;
    print_status(row, "VIP absent, deactivate pending", &a);
    let status = a.node.snapshot();
    assert_eq!(a.node.kernel_table(), Vec::<String>::new(), "VIP withdrawn");
    assert_eq!(status.resource.state, "absent", "physical observation");
    assert_eq!(status.lifecycle.phase, "deactivating");
    assert!(adverts(&pair.net, Side::A, TransferPhase::Released).is_empty());
    assert!(
        add_calls(&pair.b).is_empty(),
        "no bootstrap or acquisition on B"
    );
    assert!(
        !watchdog_a
            .calls()
            .iter()
            .any(|(_, call)| matches!(call, WatchdogCall::Disarm | WatchdogCall::MagicClose)),
        "watchdog stays armed until Inactive"
    );
    let release = wait_finished_since(&a, LifecycleKind::Deactivate, shutdown_at).await?;
    let released_at = release.finished.context("deactivate end")?;
    settle_until(BOOT_WINDOW, Duration::from_millis(50), || {
        pair.b.node.snapshot().owned
    })
    .await?;
    for (sent, packet) in side_adverts(&pair.net, Side::A) {
        if sent.at >= shutdown_at && sent.at < released_at {
            assert_eq!(
                packet.resource_state,
                AdvertResourceState::Releasing,
                "conservative advert while cleanup is unresolved at +{}ms (shutdown +{}ms, inactive +{}ms): {packet:?} calls={:?}",
                ms(origin, sent.at),
                ms(origin, shutdown_at),
                ms(origin, released_at),
                calls(&a.script)
            );
            assert_ne!(packet.transfer_phase, TransferPhase::Released);
        }
    }
    let first_released = adverts(&pair.net, Side::A, TransferPhase::Released)
        .first()
        .map(|(sent, packet)| (sent.at, packet.transfer_id))
        .context("A released after Inactive")?;
    assert!(
        first_released.0 >= released_at,
        "release only after Inactive"
    );
    let disarm_at = watchdog_a
        .calls()
        .iter()
        .find(|(_, call)| matches!(call, WatchdogCall::Disarm | WatchdogCall::MagicClose))
        .map(|(at, _)| *at)
        .context("disarm after Inactive")?;
    assert!(disarm_at >= released_at);
    assert_transfer_binding(&release, &a, &pair.b, first_released.1)?;
    print_calls(row, &a, origin);
    print_calls(row, &pair.b, origin);
    println!(
        "LIFECYCLE[{row}] RESULT vip_absent_at<=+{}ms released_advert=+{}ms (deactivate end +{}ms) disarm=+{}ms successor=activate(handoff) transfer_id={} peer_nonce==releaser instance",
        ms(shutdown_at, release.started),
        ms(origin, first_released.0),
        ms(origin, released_at),
        ms(origin, disarm_at),
        first_released.1,
    );
    a.node.stop().await?;
    pair.b.node.stop().await
}

/// F-04/F-15: both actuators name the same transfer, and the successor's
/// `GRUEZI_PEER_NONCE` is the releaser's `GRUEZI_INSTANCE_ID` (wire nonce).
fn assert_transfer_binding(
    release: &ScriptedCall,
    releaser: &LifecycleNode,
    successor: &LifecycleNode,
    transfer_id: u64,
) -> Result<()> {
    assert_eq!(
        release.operation,
        LifecycleOperation::Deactivate(DeactivationMode::HandoffRelease)
    );
    assert_eq!(release.transfer_id, Some(transfer_id));
    let takeover = calls_of(&successor.script, LifecycleKind::Activate)
        .last()
        .cloned()
        .context("successor activation")?;
    assert_eq!(
        takeover.operation,
        LifecycleOperation::Activate(ActivationMode::Handoff)
    );
    assert_eq!(takeover.transfer_id, Some(transfer_id));
    assert_eq!(
        takeover.peer_nonce.map(hex),
        Some(release.instance_id.clone())
    );
    assert_eq!(release.instance_id, releaser.node.snapshot().instance_id);
    Ok(())
}

async fn hanging_script_authority_row(policy: FailurePolicy) -> Result<()> {
    let row = format!("{policy:?}_authority_during_hanging_verify");
    let origin = Instant::now();
    let grant = scripted_authority(true);
    let external_policy = policy == FailurePolicy::External;
    let pair = if external_policy {
        external_pair(LONG_MS, &grant, false)
    } else {
        boot_strict(LONG_MS)
    };
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| {
        script.verify = Step::Hang;
        script.deactivate = Step::Delay(Duration::from_millis(1_500), 0);
    });
    let verifies = calls_of(&pair.a.script, LifecycleKind::Verify).len();
    let verify = wait_started(&pair.a, LifecycleKind::Verify, verifies).await?;
    let lapse_at = if external_policy {
        set_grant(&grant, false);
        // The next check (interval 1 s) removes the grant immediately.
        Instant::now() + Duration::from_millis(ADVERT_INTERVAL_MS)
    } else {
        pair.net.set_drop(Side::B, DropRule::All);
        let remaining = pair
            .a
            .node
            .snapshot()
            .authority
            .expires_in_ms
            .context("strict consent deadline")?;
        Instant::now() + Duration::from_millis(remaining)
    };
    let verify_deadline = verify.started + Duration::from_millis(LONG_MS);
    assert!(lapse_at < verify_deadline, "{row}: authority lapses first");
    let sent_before = side_adverts(&pair.net, Side::A).len();
    let sequence_before = pair.a.node.snapshot().sequence;
    settle_until(
        Duration::from_millis(LONG_MS),
        Duration::from_millis(10),
        || !pair.a.node.snapshot().owned,
    )
    .await?;
    // The step that first saw the revocation; it happened within that step.
    let revoked_at = Instant::now();
    let revoked_after = revoked_at - Duration::from_millis(10);
    assert!(
        revoked_at <= lapse_at + Duration::from_millis(10),
        "{row}: revoked on schedule ({}ms late)",
        ms(lapse_at, revoked_at)
    );
    assert!(
        revoked_at < verify_deadline,
        "{row}: not by the script deadline"
    );
    assert!(
        side_adverts(&pair.net, Side::A).len() > sent_before,
        "{row}: adverts kept flowing while the script hung"
    );
    assert!(pair.a.node.snapshot().sequence > sequence_before);
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    let cancelled = wait_finished(&pair.a, LifecycleKind::Verify, verifies).await?;
    assert_eq!(cancelled.outcome, Some(LifecycleOutcome::Cancelled));
    settle(Duration::from_secs(3)).await;
    let deactivate = calls_of(&pair.a.script, LifecycleKind::Deactivate)
        .last()
        .cloned()
        .context("compensating deactivate")?;
    let inactive_at = deactivate.finished.context("deactivate end")?;
    for (sent, packet) in side_adverts(&pair.net, Side::A) {
        // Adverts sent in the lapse instant itself precede the revocation
        // in that instant; every later one carries the conservative state.
        if sent.at > lapse_at && sent.at < inactive_at {
            assert_eq!(packet.resource_state, AdvertResourceState::Releasing);
            assert_eq!(packet.authority_source, AdvertAuthoritySource::None);
        }
        if matches!(
            packet.resource_state,
            AdvertResourceState::Absent | AdvertResourceState::Unmanaged
        ) && sent.at >= revoked_after
        {
            assert!(sent.at >= inactive_at, "{row}: release before Inactive");
        }
    }
    assert!(!pair.b.node.snapshot().owned);
    print_calls(&row, &pair.a, origin);
    print_status(&row, "after withdrawal", &pair.a);
    println!(
        "LIFECYCLE[{row}] RESULT lapse=+{}ms revoked=+{}ms (verify still hanging until +{}ms) vip withdrawn; adverts releasing until Inactive at +{}ms",
        ms(origin, lapse_at),
        ms(origin, revoked_at),
        ms(origin, verify_deadline),
        ms(origin, inactive_at),
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn strict_consent_expiry_and_external_grant_expiry_stay_responsive_during_hanging_script()
-> Result<()> {
    hanging_script_authority_row(FailurePolicy::Strict).await?;
    hanging_script_authority_row(FailurePolicy::External).await
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_watchdog_failure_stay_responsive() -> Result<()> {
    // Shutdown while the activation hangs: cancel, deactivate, exit in budget.
    let row = "shutdown_hanging_activation";
    let origin = Instant::now();
    let (_net, node) = boot_single(LONG_MS, NodeSeams::none());
    script_set(&node.script, |script| script.activate = Step::Hang);
    wait_started(&node, LifecycleKind::Activate, 0).await?;
    let LifecycleNode { node, script } = node;
    let kernel = node.kernel.clone();
    let stopping = Instant::now();
    node.stop().await?;
    let stopped = Instant::now();
    assert!(
        stopped - stopping <= Duration::from_millis(5_000),
        "{row}: shutdown bounded by withdraw_timeout_ms"
    );
    let activate = calls_of(&script, LifecycleKind::Activate)
        .first()
        .cloned()
        .context("activation")?;
    assert_eq!(activate.outcome, Some(LifecycleOutcome::Cancelled));
    let deactivate = calls_of(&script, LifecycleKind::Deactivate)
        .last()
        .cloned()
        .context("shutdown deactivate")?;
    assert_eq!(deactivate.outcome, Some(LifecycleOutcome::Succeeded));
    assert_eq!(
        SimulatedKernel::table_snapshot(&kernel),
        Vec::<String>::new()
    );
    print_script(row, "node-a", &script, origin);
    println!(
        "LIFECYCLE[{row}] RESULT exit after {}ms; activation cancelled; deactivate succeeded",
        (stopped - stopping).as_millis()
    );

    // Watchdog write failure while a verify hangs: revoked on the next feed.
    let row = "watchdog_failure_hanging_verify";
    let origin = Instant::now();
    let fake = FakeWatchdog::new(WatchdogFault::None);
    let (_net, node) = boot_single(
        LONG_MS,
        NodeSeams {
            authority: None,
            watchdog: Some(fake.clone()),
        },
    );
    wait_owned(&node).await?;
    script_set(&node.script, |script| script.verify = Step::Hang);
    let verifies = calls_of(&node.script, LifecycleKind::Verify).len();
    let verify = wait_started(&node, LifecycleKind::Verify, verifies).await?;
    fake.lock().fault = WatchdogFault::Write;
    let failed_at = Instant::now();
    settle_until(
        Duration::from_millis(LONG_MS),
        Duration::from_millis(10),
        || !node.node.snapshot().owned,
    )
    .await?;
    let revoked_at = Instant::now();
    assert!(revoked_at - failed_at <= Duration::from_millis(ADVERT_INTERVAL_MS));
    assert!(revoked_at < verify.started + Duration::from_millis(LONG_MS));
    assert_eq!(node.node.kernel_table(), Vec::<String>::new());
    let cancelled = wait_finished(&node, LifecycleKind::Verify, verifies).await?;
    assert_eq!(cancelled.outcome, Some(LifecycleOutcome::Cancelled));
    print_calls(row, &node, origin);
    print_status(row, "after keepalive failure", &node);
    println!(
        "LIFECYCLE[{row}] RESULT keepalive failure at +{}ms revoked +{}ms later; hanging verify cancelled",
        ms(origin, failed_at),
        (revoked_at - failed_at).as_millis()
    );
    fake.lock().fault = WatchdogFault::None;
    node.node.stop().await
}

#[tokio::test(start_paused = true)]
async fn failure_record_survives_successful_cleanup() -> Result<()> {
    let row = "failure_record";
    let origin = Instant::now();
    let pair = boot_strict(SHORT_MS);
    script_set(&pair.a.script, |script| script.activate = Step::Exit(7));
    wait_finished(&pair.a, LifecycleKind::Activate, 0).await?;
    let cleanup = wait_finished(&pair.a, LifecycleKind::Deactivate, 1).await?;
    assert_eq!(cleanup.outcome, Some(LifecycleOutcome::Succeeded));
    script_set(&pair.a.script, |script| script.activate = Step::Exit(0));
    settle(Duration::from_millis(50)).await;
    let after_cleanup = pair.a.node.snapshot();
    assert_eq!(after_cleanup.lifecycle.phase, "inactive");
    wait_owned(&pair.a).await?;
    let status = pair.a.node.snapshot();
    let failure = status
        .lifecycle
        .last_critical_failure
        .clone()
        .context("failure survives")?;
    assert_eq!(failure.event, "activate");
    assert_eq!(failure.outcome, "failed");
    assert_eq!(failure.exit_code_or_signal, Some(7));
    assert_eq!(
        after_cleanup.lifecycle.last_critical_failure,
        status.lifecycle.last_critical_failure
    );
    print_calls(row, &pair.a, origin);
    print_status(row, "owned again", &pair.a);
    println!(
        "LIFECYCLE[{row}] RESULT activate exit 7 retained through successful cleanup and a later successful activation"
    );
    pair.stop().await
}

async fn shutdown_single(
    row: &str,
    timeout_ms: u64,
    update: impl FnOnce(&mut ScriptedLifecycle),
    fail_withdrawal: bool,
) -> Result<(Duration, Vec<ScriptedCall>, HaStatus)> {
    let origin = Instant::now();
    let (_net, node) = boot_single(timeout_ms, NodeSeams::none());
    wait_owned(&node).await?;
    script_set(&node.script, update);
    if fail_withdrawal {
        node.node
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fail_del_addresses
            .push("10.0.0.10/24".to_owned());
    }
    let LifecycleNode { node, script } = node;
    let status_rx = node.status_rx.clone();
    let stopping = Instant::now();
    node.stop().await?;
    let elapsed = Instant::now() - stopping;
    let status = status_rx.borrow().clone();
    for call in calls(&script) {
        println!(
            "LIFECYCLE[{row}] t={}ms..{:?} op={:?} generation={} deadline=+{}ms(from start) outcome={:?}",
            ms(origin, call.started),
            call.finished.map(|end| ms(origin, end)),
            call.operation,
            call.generation,
            ms(call.started, call.deadline),
            call.outcome
        );
    }
    println!(
        "LIFECYCLE[{row}] RESULT cleanup took {}ms lifecycle={} failure={:?}",
        elapsed.as_millis(),
        status.lifecycle.phase,
        status.lifecycle.last_critical_failure
    );
    Ok((elapsed, calls(&script), status))
}

#[tokio::test(start_paused = true)]
async fn cleanup_fits_withdraw_deadline_with_slow_deactivate() -> Result<()> {
    let budget = Duration::from_millis(5_000);
    // Slow but successful deactivation completes inside the budget.
    let (elapsed, calls, status) = shutdown_single(
        "slow_deactivate",
        LONG_MS,
        |script| script.deactivate = Step::Delay(Duration::from_millis(3_000), 0),
        false,
    )
    .await?;
    assert!(elapsed <= budget);
    assert_eq!(status.lifecycle.phase, "inactive");
    assert_eq!(
        calls.last().and_then(|call| call.outcome),
        Some(LifecycleOutcome::Succeeded)
    );

    // P = 4.9 s exceeds the remaining budget: Deactivate is clamped (N-03) to
    // withdraw_timeout_ms minus the termination grace, so its kill and reap
    // still end inside the one deadline. A hanging deactivate is a failure.
    let (elapsed, calls, status) = shutdown_single(
        "hanging_deactivate",
        4_900,
        |script| script.deactivate = Step::Hang,
        false,
    )
    .await?;
    let deactivate = calls.last().cloned().context("deactivate")?;
    assert_eq!(
        deactivate.deadline - deactivate.started,
        Duration::from_millis(5_000 - TERMINATION_MS),
        "clamped to the remaining withdraw budget, not P"
    );
    assert_eq!(deactivate.outcome, Some(LifecycleOutcome::TimedOut));
    assert!(elapsed <= budget, "cleanup {elapsed:?} exceeded {budget:?}");
    assert_eq!(status.lifecycle.phase, "uncertain");
    assert_eq!(
        status
            .lifecycle
            .last_critical_failure
            .map(|failure| (failure.event, failure.outcome)),
        Some(("deactivate".to_owned(), "timed_out".to_owned()))
    );

    // A hanging fault notification never extends the deadline either.
    let (elapsed, calls, _status) = shutdown_single(
        "hanging_fault_notification",
        LONG_MS,
        |script| script.notify = Step::Hang,
        true,
    )
    .await?;
    assert!(
        elapsed <= budget,
        "notification extended cleanup: {elapsed:?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.operation == LifecycleOperation::Notify(HookEvent::Fault)),
        "fault notification ran through the executor"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn lifecycle_absent_preserves_all_existing_matrix_rows() -> Result<()> {
    // The full pre-existing suite runs unchanged in the same gate; these are
    // representative rows re-run here with lifecycle explicitly absent.
    super::run_startup(FailurePolicy::Strict).await?;
    super::run_startup(FailurePolicy::Available).await?;
    super::run_graceful_failover(FailurePolicy::Strict).await?;
    let harness = Harness::boot(FailurePolicy::Strict);
    settle(BOOT_WINDOW).await;
    for node in [&harness.node_a, &harness.node_b] {
        let status = node.snapshot();
        assert!(!status.lifecycle.configured);
        assert_eq!(status.lifecycle.phase, "not_configured");
    }
    for sent in harness.net.history() {
        let Some(packet) = decode_advert(&sent.bytes) else {
            continue;
        };
        let table = match sent.side {
            Side::A => &sent.kernel_a,
            Side::B => &sent.kernel_b,
        };
        // Without lifecycle the advert is the physical resource state.
        if packet.resource_state == AdvertResourceState::Releasing {
            assert!(!table.is_empty() || packet.role == crate::gruezi::ha::HaState::Init);
        }
    }
    println!(
        "LIFECYCLE[absent] RESULT strict/available startup and strict graceful failover rows unchanged; lifecycle=not_configured"
    );
    harness.stop().await
}

// ------------------------------------------------------------------- B1 rows

#[tokio::test(start_paused = true)]
async fn delayed_verify_blocks_owned_after_deadline() -> Result<()> {
    let row = "delayed_verify";
    let origin = Instant::now();
    let pair = boot_strict(SHORT_MS);
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| {
        script.verify = Step::Delay(Duration::from_secs(5), 0);
    });
    let verifies = calls_of(&pair.a.script, LifecycleKind::Verify).len();
    let verify = wait_started(&pair.a, LifecycleKind::Verify, verifies).await?;
    // Due-submit is anchored at the previous job start: it starts exactly
    // at next_verify_at, so the freshness deadline is start + D.
    let deadline = verify.started + Duration::from_millis(SHORT_MS);
    settle(
        deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_millis(10)),
    )
    .await;
    assert!(pair.a.node.snapshot().owned, "{row}: still fresh before D");
    settle(Duration::from_millis(20)).await;
    print_status(row, "after freshness deadline", &pair.a);
    let status = pair.a.node.snapshot();
    assert!(!status.owned, "{row}: owned ends at next_verify_at + D");
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    let failure = status.lifecycle.last_critical_failure.context("verify")?;
    assert_eq!(failure.event, "verify");
    print_calls(row, &pair.a, origin);
    println!(
        "LIFECYCLE[{row}] RESULT verify started +{}ms deadline +{}ms owned=false vip withdrawn kind=verify outcome={}",
        ms(origin, verify.started),
        ms(origin, deadline),
        failure.outcome
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn verify_failure_withdraws_within_withdraw_deadline() -> Result<()> {
    let row = "verify_failure";
    let origin = Instant::now();
    let pair = boot_strict(LONG_MS);
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| {
        script.verify = Step::Exit(10);
        script.deactivate = Step::Delay(Duration::from_millis(3_000), 0);
    });
    let verifies = calls_of(&pair.a.script, LifecycleKind::Verify).len();
    let verify = wait_finished(&pair.a, LifecycleKind::Verify, verifies).await?;
    let failed_at = verify.finished.context("verify end")?;
    settle(Duration::from_millis(10)).await;
    assert!(!pair.a.node.snapshot().owned);
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    let deactivates = calls_of(&pair.a.script, LifecycleKind::Deactivate).len();
    let deactivate = wait_finished(&pair.a, LifecycleKind::Deactivate, deactivates - 1).await?;
    let withdraw = Duration::from_millis(5_000);
    assert!(deactivate.started >= failed_at);
    assert!(deactivate.deadline <= failed_at + withdraw);
    assert!(deactivate.finished.context("end")? <= failed_at + withdraw);
    assert_eq!(deactivate.outcome, Some(LifecycleOutcome::Succeeded));
    let status = pair.a.node.snapshot();
    let failure = status.lifecycle.last_critical_failure.context("failure")?;
    assert_eq!(
        (failure.event.as_str(), failure.exit_code_or_signal),
        ("verify", Some(10))
    );
    print_calls(row, &pair.a, origin);
    println!(
        "LIFECYCLE[{row}] RESULT verify exit 10 at +{}ms; VIP withdrawn immediately; deactivate done +{}ms (budget 5000ms)",
        ms(origin, failed_at),
        ms(failed_at, deactivate.finished.context("end")?)
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn verify_hang_never_blocks_adverts_authority_or_status() -> Result<()> {
    let row = "verify_hang";
    let origin = Instant::now();
    let pair = boot_strict(LONG_MS);
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| script.verify = Step::Hang);
    let verifies = calls_of(&pair.a.script, LifecycleKind::Verify).len();
    let verify = wait_started(&pair.a, LifecycleKind::Verify, verifies).await?;
    let mut adverts = side_adverts(&pair.net, Side::A).len();
    let mut sequence = pair.a.node.snapshot().sequence;
    let mut min_remaining = u64::MAX;
    for second in 1..=3 {
        settle(Duration::from_millis(ADVERT_INTERVAL_MS)).await;
        let status = pair.a.node.snapshot();
        let sent = side_adverts(&pair.net, Side::A).len();
        assert!(sent > adverts, "{row}: adverts flow at +{second}s");
        assert!(
            status.sequence > sequence,
            "{row}: status flows at +{second}s"
        );
        assert_eq!(status.authority.state, "valid");
        let remaining = status.authority.expires_in_ms.context("consent")?;
        min_remaining = min_remaining.min(remaining);
        assert!(status.owned, "{row}: within next_verify_at + D");
        println!(
            "LIFECYCLE[{row}] +{second}s verify hanging: adverts={sent} sequence={} consent_expires_in_ms={remaining} owned={}",
            status.sequence, status.owned
        );
        adverts = sent;
        sequence = status.sequence;
    }
    assert!(min_remaining > 0, "consent kept advancing");
    settle_until(
        Duration::from_millis(LONG_MS),
        Duration::from_millis(10),
        || !pair.a.node.snapshot().owned,
    )
    .await?;
    let revoked_at = Instant::now();
    assert!(revoked_at >= verify.started + Duration::from_millis(LONG_MS));
    assert!(revoked_at <= verify.started + Duration::from_millis(LONG_MS + 10));
    print_calls(row, &pair.a, origin);
    println!(
        "LIFECYCLE[{row}] RESULT hanging verify +{}ms; revoked exactly at D +{}ms",
        ms(origin, verify.started),
        ms(origin, revoked_at)
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn stale_verify_cannot_restore_ownership_after_revoke() -> Result<()> {
    let row = "stale_verify";
    let origin = Instant::now();
    let grant = scripted_authority(true);
    let pair = external_pair(LONG_MS, &grant, false);
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| {
        script.verify = Step::Delay(Duration::from_millis(3_000), 0);
    });
    let verifies = calls_of(&pair.a.script, LifecycleKind::Verify).len();
    wait_started(&pair.a, LifecycleKind::Verify, verifies).await?;
    set_grant(&grant, false);
    let verify = wait_finished(&pair.a, LifecycleKind::Verify, verifies).await?;
    assert_eq!(verify.outcome, Some(LifecycleOutcome::Cancelled));
    settle(Duration::from_secs(3)).await;
    assert!(!pair.a.node.snapshot().owned);
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());

    // A successful verify whose incarnation no longer matches is a failure.
    script_set(&pair.a.script, |script| script.verify = Step::Exit(0));
    set_grant(&grant, true);
    wait_owned(&pair.a).await?;
    script_set(&pair.a.script, |script| {
        "4243:1700000999".clone_into(&mut script.incarnation);
    });
    settle_until(
        Duration::from_millis(LONG_MS * 2),
        Duration::from_millis(10),
        || !pair.a.node.snapshot().owned,
    )
    .await?;
    let status = pair.a.node.snapshot();
    assert_eq!(
        status
            .lifecycle
            .last_critical_failure
            .map(|failure| failure.event),
        Some("verify".to_owned())
    );
    assert_eq!(pair.a.node.kernel_table(), Vec::<String>::new());
    print_calls(row, &pair.a, origin);
    println!(
        "LIFECYCLE[{row}] RESULT verify in flight at revoke reported Cancelled; incarnation mismatch revoked; owned=false"
    );
    pair.stop().await
}

// ---------------------------------------------------------------- B2 core rows

/// Graceful handoff from A to B with B's activation shaped by `update`.
async fn handoff_pair(
    timeout_ms: u64,
    update_b: impl FnOnce(&mut ScriptedLifecycle),
) -> Result<LifecyclePair> {
    let pair = boot_strict(timeout_ms);
    wait_owned(&pair.a).await?;
    script_set(&pair.b.script, update_b);
    Ok(pair)
}

#[tokio::test(start_paused = true)]
async fn release_receipt_stale_after_peer_nonce_change_is_discarded() -> Result<()> {
    let row = "stale_receipt_nonce";
    let origin = Instant::now();
    let mut pair = handoff_pair(LONG_MS, |script| {
        script.activate = Step::Delay(Duration::from_millis(3_500), 0);
    })
    .await?;
    let old_instance = pair.a.node.snapshot().instance_id;
    pair.a.node.begin_shutdown();
    let takeover = wait_started(&pair.b, LifecycleKind::Activate, 0).await?;
    assert_eq!(
        takeover.operation,
        LifecycleOperation::Activate(ActivationMode::Handoff)
    );
    assert_eq!(takeover.peer_nonce.map(hex), Some(old_instance.clone()));
    // The releasing incarnation restarts mid-activation (new boot nonce).
    pair.a.node.crash();
    settle(Duration::ZERO).await;
    let kernel_a = pair.a.node.kernel.clone();
    pair.a = spawn_lifecycle_node(
        Side::A,
        pair.net.restart_endpoint(Side::A),
        lifecycle_config(Side::A, FailurePolicy::Strict, LONG_MS),
        kernel_a,
        NodeSeams::none(),
        ScriptedLifecycle::shared(),
    );
    let takeover = wait_finished(&pair.b, LifecycleKind::Activate, 0).await?;
    assert_eq!(takeover.outcome, Some(LifecycleOutcome::Cancelled));
    settle(BOOT_WINDOW).await;
    assert!(
        add_calls(&pair.b).is_empty(),
        "{row}: no VIP from a stale receipt"
    );
    assert!(!pair.b.node.snapshot().owned);
    for call in calls_of(&pair.b.script, LifecycleKind::Activate) {
        assert!(
            call.peer_nonce.map(hex) != Some(old_instance.clone())
                || call.generation == takeover.generation,
            "{row}: the stale releaser binding was reused: {call:?}"
        );
    }
    let compensation = calls_of(&pair.b.script, LifecycleKind::Deactivate)
        .into_iter()
        .find(|call| call.generation > takeover.generation)
        .context("successor compensation")?;
    assert_eq!(
        compensation.operation,
        LifecycleOperation::Deactivate(DeactivationMode::Revoke)
    );
    print_calls(row, &pair.b, origin);
    println!(
        "LIFECYCLE[{row}] RESULT handoff activation bound to {old_instance} cancelled after the releaser restarted; successor adds=0; compensation=revoke"
    );
    pair.stop().await
}

#[tokio::test(start_paused = true)]
async fn no_release_without_barrier_confirmation() -> Result<()> {
    let row = "no_release_without_barrier";
    let origin = Instant::now();
    let LifecyclePair { net, mut a, b } = handoff_pair(SHORT_MS, |_| {}).await?;
    // The adapter cannot prove the release barrier (exit 13): no receipt.
    script_set(&a.script, |script| script.deactivate = Step::Exit(13));
    a.node.begin_shutdown();
    let release = wait_finished(&a, LifecycleKind::Deactivate, 1).await?;
    assert_eq!(
        release.operation,
        LifecycleOperation::Deactivate(DeactivationMode::HandoffRelease)
    );
    assert_eq!(release.outcome, Some(LifecycleOutcome::Failed));
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 3)).await;
    print_status(row, "barrier unproven", &a);
    assert!(adverts(&net, Side::A, TransferPhase::Released).is_empty());
    let last = side_adverts(&net, Side::A)
        .last()
        .map(|(_, packet)| packet.resource_state);
    assert_eq!(last, Some(AdvertResourceState::Uncertain));
    assert_eq!(a.node.snapshot().lifecycle.phase, "uncertain");
    assert!(calls_of(&b.script, LifecycleKind::Activate).is_empty());
    assert_eq!(add_calls(&b), Vec::<Vec<String>>::new());
    print_calls(row, &a, origin);
    // The handoff never completes; the shutdown deadline still ends it.
    let stopping = Instant::now();
    a.node.stop().await?;
    assert!(Instant::now() - stopping <= Duration::from_millis(5_000));
    println!(
        "LIFECYCLE[{row}] RESULT handoff_release exit 13 -> uncertain; released_adverts=0; successor activations=0"
    );
    b.node.stop().await
}

#[tokio::test(start_paused = true)]
async fn activation_refuses_without_receipt_target() -> Result<()> {
    let row = "activation_refuses_without_receipt";
    let origin = Instant::now();
    let mut pair = handoff_pair(SHORT_MS, |script| script.activate = Step::Exit(13)).await?;
    pair.a.node.begin_shutdown();
    let refused = wait_finished(&pair.b, LifecycleKind::Activate, 0).await?;
    assert_eq!(
        refused.operation,
        LifecycleOperation::Activate(ActivationMode::Handoff)
    );
    assert_eq!(refused.outcome, Some(LifecycleOutcome::Failed));
    settle(Duration::from_millis(ADVERT_INTERVAL_MS * 3)).await;
    print_status(row, "successor refused", &pair.b);
    let status = pair.b.node.snapshot();
    assert!(!status.owned);
    assert_eq!(add_calls(&pair.b), Vec::<Vec<String>>::new());
    let failure = status.lifecycle.last_critical_failure.context("failure")?;
    assert_eq!(
        (failure.event.as_str(), failure.exit_code_or_signal),
        ("activate", Some(13))
    );
    assert!(
        calls_of(&pair.b.script, LifecycleKind::Activate)
            .iter()
            .filter(|call| call.operation == LifecycleOperation::Activate(ActivationMode::Handoff))
            .count()
            == 1,
        "the consumed transfer token is never retried as a handoff"
    );
    assert!(!adverts(&pair.net, Side::B, TransferPhase::Returned).is_empty());
    print_calls(row, &pair.b, origin);
    println!(
        "LIFECYCLE[{row}] RESULT activate(handoff) exit 13 -> compensation, transfer returned, successor adds=0"
    );
    pair.stop().await
}

// ------------------------------------------------- startup and real processes

#[test]
fn lifecycle_cleanup_requires_withdraw_deadline_to_cover_worker_bound() {
    assert!(super::super::lifecycle_cleanup_fits(
        crate::gruezi::addresses::WITHDRAW_TIMEOUT_MS
    ));
    assert!(!super::super::lifecycle_cleanup_fits(
        crate::gruezi::addresses::WITHDRAW_TIMEOUT_MS - 1
    ));
}

#[tokio::test(start_paused = true)]
async fn lifecycle_runtime_refuses_withdraw_deadline_below_worker_bound() -> Result<()> {
    let (_net, endpoint, _peer) = InMemoryNetwork::new_pair();
    let mut runtime = lifecycle_config(Side::A, FailurePolicy::Strict, SHORT_MS);
    runtime.withdraw_timeout_ms = 4_999;
    let node = spawn_lifecycle_node(
        Side::A,
        endpoint,
        runtime,
        SimulatedKernel::new_shared(),
        NodeSeams::none(),
        ScriptedLifecycle::shared(),
    );
    let error = node
        .node
        .stop()
        .await
        .err()
        .context("startup must fail closed")?;
    assert!(error.to_string().contains("ha.withdraw_timeout_ms"));
    assert!(calls(&node.script).is_empty(), "no script ever ran");
    println!("LIFECYCLE[startup] RESULT refused: {error}");
    Ok(())
}

/// Real child processes (current identity) through the real coordinator,
/// with a running clock: the environment binds the wire nonce, and the
/// baseline precedes activation, which precedes the VIP.
#[tokio::test]
async fn lifecycle_real_scripts_bind_instance_and_order_activation_before_vip() -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};
    let dir = std::env::temp_dir().join(format!("gruezi-lifecycle-runtime-{}", std::process::id()));
    fs::create_dir(&dir)?;
    let log = dir.join("lifecycle.log");
    let activate = dir.join("activate");
    let deactivate = dir.join("deactivate");
    fs::write(
        &activate,
        format!(
            "#!/bin/sh\nprintf '%s %s %s %s %s\\n' \"$GRUEZI_EVENT\" \"${{GRUEZI_ACTIVATION_MODE:-none}}\" \"$GRUEZI_GENERATION\" \"$GRUEZI_INSTANCE_ID\" \"$GRUEZI_TRANSFER_ID\" >> '{}'\necho db_incarnation=real-1\n",
            log.display()
        ),
    )?;
    fs::write(
        &deactivate,
        format!(
            "#!/bin/sh\nprintf '%s %s %s %s\\n' \"$GRUEZI_EVENT\" \"$GRUEZI_DEACTIVATE_MODE\" \"$GRUEZI_GENERATION\" \"$GRUEZI_INSTANCE_ID\" >> '{}'\n",
            log.display()
        ),
    )?;
    for script in [&activate, &deactivate] {
        fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    }
    let (_net, endpoint, _peer) = InMemoryNetwork::new_pair();
    let mut runtime = node_config(Side::A, FailurePolicy::Available);
    runtime.advert_interval_ms = 100;
    runtime.hold_down_ms = 100;
    runtime.lifecycle = Some(LifecycleConfig {
        activate_command: activate.display().to_string(),
        deactivate_command: deactivate.display().to_string(),
        timeout_ms: 2_000,
    });
    let kernel = SimulatedKernel::new_shared();
    let node = super::spawn_with_seams(
        Side::A,
        endpoint,
        runtime,
        super::super::Seams {
            commands: CommandRunner::Simulated(kernel.clone()),
            authority: AuthorityCommandRunner::System,
            watchdog: WatchdogDevice::default(),
            lifecycle: super::super::LifecycleRunner::Process,
        },
        kernel,
    );
    let mut status_rx = node.status_rx.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !status_rx.borrow_and_update().owned {
            if status_rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .context("real lifecycle owner")?;
    let instance = node.snapshot().instance_id;
    assert_eq!(node.kernel_table(), vec!["eth0 10.0.0.10/24".to_owned()]);
    node.stop().await?;
    let lines: Vec<String> = fs::read_to_string(&log)?
        .lines()
        .map(str::to_owned)
        .collect();
    println!("LIFECYCLE[real] log: {lines:#?}");
    let baseline = format!("deactivate baseline 0 {instance}");
    let activation = format!("activate cold 1 {instance} 0");
    assert_eq!(lines.first(), Some(&baseline), "baseline runs first");
    assert_eq!(lines.get(1), Some(&activation), "then a cold activation");
    assert!(
        lines
            .last()
            .is_some_and(|line| line.starts_with("deactivate revoke ") && line.ends_with(&instance)),
        "shutdown deactivation bound to the same instance"
    );
    fs::remove_dir_all(&dir)?;
    Ok(())
}
