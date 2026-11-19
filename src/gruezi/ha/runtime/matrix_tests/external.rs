//! Every row runs the actual coordinator, handoff, command deadlines, and
//! serialized kernel worker. The scripted arbiter gives a grant to only one
//! node at a time. Successful fencing removes the crashed peer's residual VIP.

use super::*;
use crate::{
    config::AuthorityConfig,
    gruezi::{
        ha::runtime::authority_commands::{
            AuthorityCommandRunner, ScriptedAuthority, ScriptedOutcome,
        },
        ownership::FencingState,
    },
};

const GRANT_TTL_MS: u64 = 20_000;

type Script = Arc<Mutex<ScriptedAuthority>>;

fn script_lock(script: &Script) -> std::sync::MutexGuard<'_, ScriptedAuthority> {
    script.lock().unwrap_or_else(PoisonError::into_inner)
}

fn external_config(side: Side) -> HaRuntimeConfig {
    let mut config = node_config(side, FailurePolicy::External);
    config.authority = Some(AuthorityConfig {
        check_command: "authority-check".to_owned(),
        fence_command: Some("authority-fence".to_owned()),
        interval_ms: Some(ADVERT_INTERVAL_MS),
        grant_ttl_ms: GRANT_TTL_MS,
    });
    config
}

struct ExternalHarness {
    pair: Harness,
    a: Script,
    b: Script,
}

impl ExternalHarness {
    fn boot() -> Self {
        Self::boot_configs(external_config(Side::A), external_config(Side::B))
    }

    fn boot_configs(a: HaRuntimeConfig, b: HaRuntimeConfig) -> Self {
        let (net, endpoint_a, endpoint_b) = InMemoryNetwork::new_pair();
        let script_a = Arc::new(Mutex::new(ScriptedAuthority {
            check: ScriptedOutcome::Exit(true),
            ..ScriptedAuthority::default()
        }));
        let script_b = Arc::new(Mutex::new(ScriptedAuthority::default()));
        let node_a = spawn_with_authority(
            Side::A,
            endpoint_a,
            a,
            SimulatedKernel::new_shared(),
            AuthorityCommandRunner::Scripted(script_a.clone()),
        );
        let node_b = spawn_with_authority(
            Side::B,
            endpoint_b,
            b,
            SimulatedKernel::new_shared(),
            AuthorityCommandRunner::Scripted(script_b.clone()),
        );
        script_lock(&script_a).fenced_kernel = Some(node_b.kernel.clone());
        script_lock(&script_b).fenced_kernel = Some(node_a.kernel.clone());
        net.capture_kernels(node_a.kernel.clone(), node_b.kernel.clone());
        Self {
            pair: Harness {
                net,
                node_a,
                node_b,
            },
            a: script_a,
            b: script_b,
        }
    }

    fn grant_b(&self) {
        script_lock(&self.a).check = ScriptedOutcome::Exit(false);
        script_lock(&self.b).check = ScriptedOutcome::Exit(true);
    }

    fn print(&self, row: &str, phase: &str) {
        print_matrix_state(row, phase, &self.pair.node_a);
        print_matrix_state(row, phase, &self.pair.node_b);
        println!(
            "EXTERNAL[{row}] {phase} check/fence command starts A={:?} B={:?}",
            script_lock(&self.a).calls,
            script_lock(&self.b).calls,
        );
    }

    async fn stop(self) -> Result<()> {
        self.pair.stop().await
    }
}

fn assert_external_owner(owner: &Node, other: &Node) {
    let status = owner.snapshot();
    assert!(status.owned, "{} must own: {status:?}", owner.name);
    assert_eq!(status.authority.source, Some(AuthoritySource::External));
    assert!(status.authority.expires_in_ms.is_some_and(|ms| ms > 0));
    assert_eq!(owner.kernel_table(), vec!["eth0 10.0.0.10/24".to_owned()]);
    assert_eq!(other.kernel_table(), Vec::<String>::new());
}

fn assert_no_acquisition(node: &Node) {
    assert!(!node.snapshot().owned);
    assert_eq!(
        kernel_add_calls(node),
        Vec::<Vec<String>>::new(),
        "no acquisition is permitted"
    );
    assert_eq!(node.kernel_table(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn matrix_external_startup() -> Result<()> {
    let harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    assert_no_acquisition(&harness.pair.node_b);
    assert_eq!(
        harness.pair.node_a.snapshot().authority.fencing,
        FencingState::None
    );
    assert!(
        script_lock(&harness.a)
            .calls
            .iter()
            .all(|(_, peer, _)| peer.is_none())
    );
    let check_starts: Vec<_> = script_lock(&harness.a)
        .calls
        .iter()
        .map(|(_, _, started)| *started)
        .collect();
    assert!(check_starts.len() >= 8);
    assert!(check_starts.windows(2).all(|pair| {
        match pair {
            [previous, next] => *next - *previous == Duration::from_millis(ADVERT_INTERVAL_MS),
            _ => false,
        }
    }));
    harness.print("startup", "fresh grant and bootstrap");
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_graceful_failover() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.begin_shutdown();
    settle(Duration::from_secs(1)).await;
    assert_eq!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
    harness.grant_b();
    settle(Duration::from_secs(3)).await;
    assert_external_owner(&harness.pair.node_b, &harness.pair.node_a);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::None
    );
    assert_ne!(
        adverts(&harness.pair.net, Side::A, TransferPhase::Released).len(),
        0
    );
    assert_advert_order(&harness.pair.net);
    harness.print("graceful_failover", "grant plus peer-confirmed release");
    harness.stop().await
}

async fn active_crash(outcome: ScriptedOutcome, row: &str) -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    harness.pair.node_a.crash();
    harness.grant_b();
    script_lock(&harness.b).fence = outcome;
    let acquisition_gate = if matches!(outcome, ScriptedOutcome::Delay(_, true)) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        harness
            .pair
            .node_b
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .gate_next_add = Some(CommandGate {
            entered: entered_tx,
            release: release_rx,
        });
        Some((entered_rx, release_tx))
    } else {
        None
    };
    settle(Duration::from_millis(4_500)).await;
    assert_no_acquisition(&harness.pair.node_b);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing.as_str(),
        "requested"
    );
    assert!(
        harness
            .pair
            .node_b
            .snapshot()
            .authority
            .grant_expires_in_ms
            .is_some_and(|ms| ms > 0)
    );
    harness.print(row, "grant valid; fencing requested; NO acquisition");
    if let ScriptedOutcome::Delay(_, true) = outcome {
        settle(Duration::from_secs(2)).await;
        let (entered, release) = acquisition_gate.context("acquisition gate configured")?;
        entered.await.context("acquisition must reach gate")?;
        assert_eq!(
            harness.pair.node_b.snapshot().authority.fencing,
            FencingState::Confirmed
        );
        assert_eq!(
            harness.pair.node_b.snapshot().authority.fencing.as_str(),
            "confirmed"
        );
        assert!(!harness.pair.node_b.snapshot().owned);
        assert_eq!(harness.pair.node_b.kernel_table(), Vec::<String>::new());
        assert_eq!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
        harness.print(
            row,
            "grant valid; fencing confirmed; acquisition gated BEFORE kernel add",
        );
        let _ = release.send(Ok(()));
        settle(Duration::ZERO).await;
        assert_external_owner(&harness.pair.node_b, &harness.pair.node_a);
        assert_eq!(
            harness.pair.node_b.snapshot().authority.fencing,
            FencingState::Confirmed
        );
        harness.print(
            row,
            "fencing confirmed BEFORE acquisition; predecessor kernel empty",
        );
    } else {
        settle(Duration::from_secs(4)).await;
        assert_no_acquisition(&harness.pair.node_b);
        assert_eq!(
            harness.pair.node_b.snapshot().authority.fencing,
            FencingState::Requested
        );
        assert_ne!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
        harness.print(row, "fencing requested, NOT confirmed; NO acquisition");
    }
    assert!(
        script_lock(&harness.b).calls.iter().any(|(path, peer, _)| {
            path == "authority-fence" && peer.as_deref() == Some("node-a")
        })
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_active_crash_fencing_confirmed() -> Result<()> {
    active_crash(
        ScriptedOutcome::Delay(Duration::from_secs(2), true),
        "active_crash_fencing_confirmed",
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_active_crash_fencing_failed() -> Result<()> {
    active_crash(ScriptedOutcome::Exit(false), "active_crash_fencing_failed").await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_active_crash_fencing_timeout() -> Result<()> {
    active_crash(ScriptedOutcome::Timeout, "active_crash_fencing_timeout").await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_standby_crash() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_b.crash();
    settle(Duration::from_secs(30)).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    harness.print("standby_crash", "grant renewal continues without peer");
    harness.stop().await
}

async fn loss_row(a_to_b: DropRule, b_to_a: DropRule, row: &str) -> Result<()> {
    let harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.net.set_drop(Side::A, a_to_b);
    harness.pair.net.set_drop(Side::B, b_to_a);
    settle(Duration::from_secs(40)).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    assert_no_acquisition(&harness.pair.node_b);
    harness.print(
        row,
        "silence grants nothing; only external grant owner serves",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_full_partition() -> Result<()> {
    loss_row(DropRule::All, DropRule::All, "full_partition").await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_one_way_a_to_b() -> Result<()> {
    loss_row(DropRule::All, DropRule::None, "one_way_a_to_b").await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_one_way_b_to_a() -> Result<()> {
    loss_row(DropRule::None, DropRule::All, "one_way_b_to_a").await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_failed_withdrawal() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    {
        let mut kernel = harness
            .pair
            .node_a
            .kernel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        kernel.fail_del_addresses.push("10.0.0.10/24".to_owned());
        kernel.fail_show = true;
    }
    harness.pair.node_a.begin_shutdown();
    harness.grant_b();
    settle(Duration::from_secs(30)).await;
    assert_eq!(
        adverts(&harness.pair.net, Side::A, TransferPhase::Released).len(),
        0
    );
    assert_no_acquisition(&harness.pair.node_b);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    assert_ne!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
    harness.print(
        "failed_withdrawal",
        "no release or confirmed fencing; NO acquisition",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_recovery_failback() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.crash();
    harness.grant_b();
    script_lock(&harness.b).fence = ScriptedOutcome::Exit(true);
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_b, &harness.pair.node_a);
    harness.pair.node_a = spawn_with_authority(
        Side::A,
        harness.pair.net.restart_endpoint(Side::A),
        external_config(Side::A),
        harness.pair.node_a.kernel.clone(),
        AuthorityCommandRunner::Scripted(harness.a.clone()),
    );
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_b, &harness.pair.node_a);
    harness.pair.node_b.begin_shutdown();
    settle(Duration::from_secs(1)).await;
    script_lock(&harness.b).check = ScriptedOutcome::Exit(false);
    script_lock(&harness.a).check = ScriptedOutcome::Exit(true);
    settle(Duration::from_secs(3)).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    assert_advert_order(&harness.pair.net);
    harness.print(
        "recovery_failback",
        "fresh grant plus release after restart",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_stale_grant() -> Result<()> {
    let mut config_a = external_config(Side::A);
    // Cutoff is 1s from command start (8s TTL less 5s withdrawal
    // budget and 2s safety margin). A hung renewal lasts 3s.
    config_a.authority = Some(AuthorityConfig {
        grant_ttl_ms: 8_000,
        interval_ms: Some(500),
        ..config_a.authority.context("authority configured")?
    });
    let harness = ExternalHarness::boot_configs(config_a, external_config(Side::B));
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
    harness.print("stale_grant", "before expiry with confirmed ownership");
    script_lock(&harness.a).check = ScriptedOutcome::Timeout;
    settle(Duration::from_millis(1_100)).await;
    assert!(!harness.pair.node_a.snapshot().owned);
    assert_eq!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
    harness.print(
        "stale_grant",
        "cutoff reached during hung renewal; withdrawal confirmed",
    );
    settle(Duration::from_millis(GRANT_TTL_MS * 2)).await;
    assert!(!harness.pair.node_a.snapshot().owned);
    assert_no_acquisition(&harness.pair.node_b);
    harness.print("stale_grant", "past raw TTL; stale grant never authorizes");
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_grant_expiry_is_not_fencing() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.crash();
    harness.grant_b();
    settle(Duration::from_millis(GRANT_TTL_MS * 3)).await;
    assert_no_acquisition(&harness.pair.node_b);
    assert_ne!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    harness.print(
        "grant_expiry_is_not_fencing",
        "predecessor still serves past TTL; successor blocked",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_missing_fence_command() -> Result<()> {
    let mut config_b = external_config(Side::B);
    if let Some(authority) = &mut config_b.authority {
        authority.fence_command = None;
    }
    let mut harness = ExternalHarness::boot_configs(external_config(Side::A), config_b);
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.crash();
    harness.grant_b();
    settle(Duration::from_secs(30)).await;
    assert_no_acquisition(&harness.pair.node_b);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    assert!(
        script_lock(&harness.b)
            .calls
            .iter()
            .all(|(_, peer, _)| peer.is_none())
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_nonzero_check_withdraws() -> Result<()> {
    let harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    script_lock(&harness.a).check = ScriptedOutcome::Exit(false);
    settle(Duration::from_millis(1_100)).await;
    assert!(!harness.pair.node_a.snapshot().owned);
    assert_eq!(harness.pair.node_a.kernel_table(), Vec::<String>::new());
    assert_no_acquisition(&harness.pair.node_b);
    harness.print(
        "nonzero_check_withdraws",
        "nonzero exit immediately revokes grant",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_lone_startup_needs_fencing() -> Result<()> {
    let harness = ExternalHarness::boot();
    harness.pair.net.set_drop(Side::A, DropRule::All);
    harness.pair.net.set_drop(Side::B, DropRule::All);
    settle(Duration::from_secs(30)).await;
    assert_no_acquisition(&harness.pair.node_a);
    assert_no_acquisition(&harness.pair.node_b);
    assert_eq!(
        harness.pair.node_a.snapshot().authority.fencing,
        FencingState::Requested
    );
    harness.print(
        "lone_startup",
        "live grant without release or fencing cannot acquire",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_delayed_check_uses_command_start() -> Result<()> {
    let mut a = external_config(Side::A);
    if let Some(config) = &mut a.authority {
        config.grant_ttl_ms = 8_000;
    }
    let harness = ExternalHarness::boot_configs(a, external_config(Side::B));
    script_lock(&harness.a).check = ScriptedOutcome::Delay(Duration::from_millis(1_500), true);
    settle(Duration::from_secs(30)).await;
    assert_no_acquisition(&harness.pair.node_a);
    assert_no_acquisition(&harness.pair.node_b);
    assert!(
        harness
            .pair
            .node_a
            .snapshot()
            .authority
            .grant_expires_in_ms
            .is_none()
    );
    harness.print(
        "delayed_check",
        "exit 0 after conservative cutoff cannot create a fresh grant",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_fence_proof_does_not_survive_grant_revocation() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.crash();
    harness.grant_b();
    script_lock(&harness.b).fence = ScriptedOutcome::Exit(true);
    settle(BOOT_WINDOW).await;
    assert_external_owner(&harness.pair.node_b, &harness.pair.node_a);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Confirmed
    );
    let adds = kernel_add_calls(&harness.pair.node_b).len();
    script_lock(&harness.b).check = ScriptedOutcome::Exit(false);
    settle(Duration::from_millis(1_100)).await;
    assert!(!harness.pair.node_b.snapshot().owned);
    assert_eq!(harness.pair.node_b.kernel_table(), Vec::<String>::new());
    script_lock(&harness.b).check = ScriptedOutcome::Exit(true);
    script_lock(&harness.b).fence = ScriptedOutcome::Exit(false);
    settle(BOOT_WINDOW).await;
    assert!(!harness.pair.node_b.snapshot().owned);
    assert_eq!(kernel_add_calls(&harness.pair.node_b).len(), adds);
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    harness.print(
        "fence_proof_revoked",
        "new grant needs new proof; old confirmed fence cannot authorize",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_peer_activity_invalidates_inflight_fence() -> Result<()> {
    let mut harness = ExternalHarness::boot();
    settle(BOOT_WINDOW).await;
    harness.pair.node_a.crash();
    harness.grant_b();
    script_lock(&harness.b).fence = ScriptedOutcome::Delay(Duration::from_millis(2_900), true);
    settle(Duration::from_millis(4_500)).await;
    assert_eq!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Requested
    );
    harness.pair.node_a = spawn_with_authority(
        Side::A,
        harness.pair.net.restart_endpoint(Side::A),
        external_config(Side::A),
        harness.pair.node_a.kernel.clone(),
        AuthorityCommandRunner::Scripted(harness.a.clone()),
    );
    // The new peer establishes at 14.5s. Delay the in-flight completion until
    // after this authenticated activity, which must invalidate that proof.
    settle(Duration::from_secs(3)).await;
    assert_no_acquisition(&harness.pair.node_b);
    assert_ne!(
        harness.pair.node_b.snapshot().authority.fencing,
        FencingState::Confirmed
    );
    harness.print(
        "peer_activity",
        "fresh peer activity prevents reusing an old fence result",
    );
    harness.stop().await
}

#[tokio::test(start_paused = true)]
async fn matrix_external_i4_loss_duplicate_reorder_crash_restart() -> Result<()> {
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
            let mut harness = ExternalHarness::boot();
            settle(BOOT_WINDOW).await;
            harness.pair.node_a.begin_shutdown();
            harness.grant_b();
            let schedule = format!("external/{first}/{second}");
            for event in [first, second] {
                match event {
                    "loss-a" => harness.pair.net.set_drop(Side::A, DropRule::All),
                    "loss-b" => harness.pair.net.set_drop(Side::B, DropRule::All),
                    "duplicate" => {
                        harness.pair.net.set_duplicate(Side::A, 2);
                        harness.pair.net.set_duplicate(Side::B, 2);
                    }
                    "reorder" => {
                        harness.pair.net.set_reorder(Side::A, 1);
                        harness.pair.net.set_reorder(Side::B, 1);
                    }
                    "crash-a" => harness.pair.node_a.crash(),
                    "restart-a" | "restart-b" => {
                        let (side, node, script) = if event == "restart-a" {
                            (Side::A, &mut harness.pair.node_a, harness.a.clone())
                        } else {
                            (Side::B, &mut harness.pair.node_b, harness.b.clone())
                        };
                        node.crash();
                        settle(Duration::ZERO).await;
                        *node = spawn_with_authority(
                            side,
                            harness.pair.net.restart_endpoint(side),
                            external_config(side),
                            node.kernel.clone(),
                            AuthorityCommandRunner::Scripted(script),
                        );
                    }
                    _ => return Err(anyhow!("unknown external schedule event")),
                }
                for _ in 0..16 {
                    settle(Duration::from_millis(250)).await;
                    assert_no_two_kernel_owners(&harness.pair, &schedule);
                }
            }
            harness.pair.net.set_drop(Side::A, DropRule::None);
            harness.pair.net.set_drop(Side::B, DropRule::None);
            settle(BOOT_WINDOW).await;
            assert_no_two_kernel_owners(&harness.pair, &schedule);
            assert_advert_order(&harness.pair.net);
            harness.stop().await?;
            explored += 1;
        }
    }
    assert_eq!(explored, 49);
    println!(
        "EXTERNAL I4: {explored} loss/duplicate/reorder/crash/restart schedules; no two kernel owners"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn matrix_external_out_of_order_checks_preserve_newest_result() -> Result<()> {
    for old_success in [true, false] {
        let harness = ExternalHarness::boot();
        settle(BOOT_WINDOW).await;
        assert_external_owner(&harness.pair.node_a, &harness.pair.node_b);
        let term = harness.pair.node_a.snapshot().authority.term;
        let adds = kernel_add_calls(&harness.pair.node_a).len();
        script_lock(&harness.a).check =
            ScriptedOutcome::Delay(Duration::from_millis(2_500), old_success);
        settle(Duration::from_millis(1_100)).await;
        script_lock(&harness.a).check = ScriptedOutcome::Exit(!old_success);
        settle(Duration::from_millis(1_500)).await;
        assert_eq!(harness.pair.node_a.snapshot().owned, !old_success);
        settle(Duration::from_millis(1_100)).await;
        assert_eq!(harness.pair.node_a.snapshot().owned, !old_success);
        assert_eq!(kernel_add_calls(&harness.pair.node_a).len(), adds);
        if !old_success {
            assert_eq!(harness.pair.node_a.snapshot().authority.term, term);
        }
        // The 9s check is still running when the 10s check starts.
        let starts: Vec<_> = script_lock(&harness.a)
            .calls
            .iter()
            .filter(|(_, peer, _)| peer.is_none())
            .map(|(_, _, at)| *at)
            .collect();
        assert!(starts.windows(2).all(|pair| {
            matches!(pair, [previous, next] if *next - *previous == Duration::from_secs(1))
        }));
        harness.print(
            "out_of_order_checks",
            "late older completion cannot reverse a newer result",
        );
        harness.stop().await?;
    }
    Ok(())
}
