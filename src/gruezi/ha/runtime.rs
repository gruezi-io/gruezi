use super::{
    config::HaRuntimeConfig,
    election::{
        HaDecision, HaDecisionReason, HaState, PeerObservation, desired_state,
        has_live_active_peer, outranks,
    },
    protocol::{
        AdvertAuthoritySource, AdvertResourceState, HaPacket, PacketRejection, RECEIVE_BUFFER_LEN,
    },
    session::{Admission, AdvertState, PeerTracker, boot_nonce},
    status::{HaStatus, TransitionTrigger, publish_loop_status, saturating_elapsed_ms},
};
use crate::{
    config::FailurePolicy,
    gruezi::{
        addresses::{
            AddressManager, CommandRunner, ResourceReport, ResourceReportKind,
            ResourceWorkerHandle, WITHDRAW_TIMEOUT_MS,
        },
        application::ApplicationPhase,
        hooks::{
            CANCEL_GRACE_MS, HookContext, HookEvent, LifecycleEvent, LifecycleExecutor,
            LifecycleKind, LifecycleOutcome, spawn_hook,
        },
        ownership::{
            LifecycleBinding, OwnershipController, ReportEffect, ResourceManagement,
            RoleTransitionEffect,
        },
        watchdog::WatchdogDevice,
    },
};
use anyhow::{Context, Result, anyhow, bail};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    collections::VecDeque,
    future::Future,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
};
use tracing::{debug, info, warn};

mod authority_commands;
mod handoff;
/// Deterministic in-process datagram fabric used by the failure matrix tests
/// (design spec 8a: an enum seam, never a trait object).
#[cfg(test)]
mod in_memory;
#[cfg(test)]
mod matrix_tests;

/// Number of own advert send records kept for consent bookkeeping.
const SENT_ADVERT_HISTORY: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct HaCounters {
    pub(super) packets_sent: u64,
    pub(super) packets_received: u64,
    pub(super) invalid_packets: u64,
    pub(super) auth_failures: u64,
    pub(super) duplicate_node_id_packets: u64,
}

#[derive(Debug)]
pub(super) struct HaLoopState {
    pub(super) startup_at: Instant,
    pub(super) state: HaState,
    // Keep this sticky while a live peer remains Active so an asymmetric-loss
    // demotion does not immediately flip back on the next advertisement.
    pub(super) active_peer_conflict: bool,
    pub(super) decision_reason: HaDecisionReason,
    pub(super) last_transition_reason: Option<HaDecisionReason>,
    pub(super) last_transition_at: Option<Instant>,
    // Fed only with adverts the peer tracker classified as fresh.
    pub(super) peer_observation: Option<PeerObservation>,
    pub(super) peer: PeerTracker,
    pub(super) counters: HaCounters,
    // Own advert send records (seq -> send time) for bilateral consent
    // (design spec 4.4): a fresh ack_seq resolves against this history.
    pub(super) sent_adverts: VecDeque<(u64, tokio::time::Instant)>,
    pub(super) next_advert_at: tokio::time::Instant,
    pub(super) next_reconcile_at: tokio::time::Instant,
    pub(super) ownership: OwnershipController,
    pub(super) last_transition_trigger: Option<TransitionTrigger>,
    pub(super) handoff: handoff::Handoff,
    shutdown_deadline: Option<tokio::time::Instant>,
    // End of the latest policy evaluation; a larger gap than the follower
    // deadline means this process was stopped (stall detection).
    last_refresh_at: tokio::time::Instant,
    stall_resync: Option<StallResync>,
}

/// A stalled `available` owner yields and stays standby until the peer
/// acknowledges an advert sent after the stall, so the peer state it then
/// elects against is current instead of queued from before the stall.
#[derive(Debug, Clone, Copy)]
struct StallResync {
    /// First own sequence number advertised after the stall was handled.
    seq: u64,
    /// Without such an acknowledgement ordinary election resumes here.
    until: tokio::time::Instant,
}

/// Datagram transport seam (design spec 8a: an enum, never a trait object).
/// `Udp` is the production transport; `InMemory` is the deterministic
/// two-node network with loss/duplicate/reorder knobs used by the harness
/// under `tokio::time::pause`.
pub(super) enum Transport {
    Udp,
    #[cfg(test)]
    InMemory(in_memory::InMemoryEndpoint),
}

enum DatagramIo {
    Udp(UdpSocket),
    #[cfg(test)]
    InMemory(in_memory::InMemoryEndpoint),
}

impl DatagramIo {
    async fn recv(&self, buffer: &mut [u8]) -> std::io::Result<(usize, std::net::SocketAddr)> {
        match self {
            Self::Udp(socket) => socket.recv_from(buffer).await,
            #[cfg(test)]
            Self::InMemory(endpoint) => endpoint.recv(buffer).await,
        }
    }

    async fn send(&self, bytes: &[u8], target: std::net::SocketAddr) -> std::io::Result<usize> {
        match self {
            Self::Udp(socket) => socket.send_to(bytes, target).await,
            #[cfg(test)]
            Self::InMemory(endpoint) => Ok(endpoint.send(bytes)),
        }
    }
}

/// Run the HA control loop.
///
/// This keeps the process alive, exchanges UDP advertisements with the peer,
/// tracks peer liveness, and drives the local HA state machine.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound or packet I/O fails.
pub async fn run(runtime: HaRuntimeConfig) -> Result<()> {
    run_with_status(runtime, None, crate::gruezi::signal::shutdown_signal()).await
}

/// Run the HA control loop until the provided shutdown future resolves.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound or packet I/O fails.
pub async fn run_until<F, T>(runtime: HaRuntimeConfig, shutdown: F) -> Result<()>
where
    F: Future<Output = T>,
{
    run_with_status(runtime, None, shutdown).await
}

/// Run the HA control loop and publish snapshots to a watch channel.
///
/// This is primarily intended for integration tests and future API/status hooks.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound or packet I/O fails.
pub async fn run_with_status<F, T>(
    runtime: HaRuntimeConfig,
    status_tx: Option<watch::Sender<HaStatus>>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = T>,
{
    run_with_transport(
        runtime,
        Transport::Udp,
        CommandRunner::System,
        status_tx,
        shutdown,
    )
    .await
}

/// Run the HA control loop over the given transport and command runner.
///
/// `Transport::InMemory` and a scripted command runner exist for the
/// deterministic harness (design spec 8a); production entry points always use
/// UDP and system commands. No state here survives a restart.
///
/// # Errors
///
/// Returns an error if the socket cannot be bound or packet I/O fails.
pub(super) async fn run_with_transport<F, T>(
    runtime: HaRuntimeConfig,
    transport: Transport,
    command_runner: CommandRunner,
    status_tx: Option<watch::Sender<HaStatus>>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = T>,
{
    run_with_authority_commands(
        runtime,
        transport,
        command_runner,
        authority_commands::AuthorityCommandRunner::System,
        status_tx,
        shutdown,
    )
    .await
}

async fn run_with_authority_commands<F, T>(
    runtime: HaRuntimeConfig,
    transport: Transport,
    command_runner: CommandRunner,
    authority_runner: authority_commands::AuthorityCommandRunner,
    status_tx: Option<watch::Sender<HaStatus>>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = T>,
{
    run_with_watchdog(
        runtime,
        transport,
        command_runner,
        authority_runner,
        WatchdogDevice::default(),
        status_tx,
        shutdown,
    )
    .await
}

async fn run_with_watchdog<F, T>(
    runtime: HaRuntimeConfig,
    transport: Transport,
    runner: CommandRunner,
    authority_runner: authority_commands::AuthorityCommandRunner,
    watchdog: WatchdogDevice,
    status_tx: Option<watch::Sender<HaStatus>>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = T>,
{
    run_with_seams(
        runtime,
        transport,
        Seams {
            commands: runner,
            authority: authority_runner,
            watchdog,
            lifecycle: LifecycleRunner::System,
        },
        status_tx,
        shutdown,
    )
    .await
}

/// Lifecycle script execution seam (design spec 8a: an enum, never a trait
/// object). Production always runs the packaged-identity executor.
enum LifecycleRunner {
    System,
    /// Real subprocesses at the test process identity, with a running clock.
    #[cfg(test)]
    Process,
    /// The real executor queue with scripted jobs in virtual time.
    #[cfg(test)]
    Scripted(std::sync::Arc<std::sync::Mutex<crate::gruezi::hooks::scripted::ScriptedLifecycle>>),
}

/// Every process-level side effect the coordinator drives.
struct Seams {
    commands: CommandRunner,
    authority: authority_commands::AuthorityCommandRunner,
    watchdog: WatchdogDevice,
    lifecycle: LifecycleRunner,
}

type LifecycleEvents = Option<mpsc::UnboundedReceiver<LifecycleEvent>>;

/// Everything the coordinator loop owns once transport and executor run.
struct Started {
    io: DatagramIo,
    peer_addr: std::net::SocketAddr,
    worker: ResourceWorkerHandle,
    loop_state: HaLoopState,
    lifecycle_events: LifecycleEvents,
    authority_commands: authority_commands::AuthorityCommands,
}

async fn start_runtime(
    runtime: &HaRuntimeConfig,
    transport: Transport,
    seams: Seams,
    status_tx: Option<&watch::Sender<HaStatus>>,
) -> Result<Started> {
    let (io, local_bind, peer_addr) = open_transport(runtime, transport).await?;
    // One boot nonce is both the wire sender_nonce and the actuator's
    // GRUEZI_INSTANCE_ID (errata F-04); never draw a second identity.
    let nonce = boot_nonce()?;
    let (executor, lifecycle_events) = spawn_lifecycle(runtime, nonce, &seams.lifecycle)?
        .map_or((None, None), |(executor, events)| {
            (Some(executor), Some(events))
        });
    let peer = PeerTracker::new(runtime, peer_addr.ip(), nonce)?;
    let (worker, mut loop_state) = spawn_worker(runtime, seams.commands, status_tx, peer, executor);
    loop_state
        .ownership
        .configure_watchdog(runtime, seams.watchdog);
    log_runtime_start(runtime, &local_bind, peer_addr);
    Ok(Started {
        io,
        peer_addr,
        worker,
        loop_state,
        lifecycle_events,
        authority_commands: authority_commands::AuthorityCommands::new(seams.authority),
    })
}

async fn run_with_seams<F, T>(
    runtime: HaRuntimeConfig,
    transport: Transport,
    seams: Seams,
    status_tx: Option<watch::Sender<HaStatus>>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = T>,
{
    let Started {
        io,
        peer_addr,
        mut worker,
        mut loop_state,
        mut lifecycle_events,
        mut authority_commands,
    } = start_runtime(&runtime, transport, seams, status_tx.as_ref()).await?;
    let mut buffer = [0_u8; RECEIVE_BUFFER_LEN];
    tokio::pin!(shutdown);

    loop {
        evaluate_iteration(
            &runtime,
            status_tx.as_ref(),
            &mut loop_state,
            &mut worker,
            &mut authority_commands,
        );
        if loop_state.shutdown_deadline.is_some_and(|deadline| {
            tokio::time::Instant::now() >= deadline || loop_state.handoff.acknowledged
        }) {
            exit_cleanup(
                &runtime,
                status_tx.as_ref(),
                &mut loop_state,
                &mut worker,
                &mut lifecycle_events,
                &mut authority_commands,
                false,
            )
            .await;
            break;
        }

        let verify_wake = sleep_until_optional(verification_wake(&loop_state.ownership));
        let advert_sleep = tokio::time::sleep_until(loop_state.next_advert_at);
        let reconcile_sleep = tokio::time::sleep_until(loop_state.next_reconcile_at);
        let deadline_wake =
            sleep_until_optional(next_deadline(&runtime, &loop_state, &authority_commands));
        tokio::pin!(advert_sleep, reconcile_sleep, deadline_wake, verify_wake);

        let event = tokio::select! {
            () = authority_commands.complete(
                &runtime, &mut loop_state.ownership, loop_state.peer.last_fresh_at(),
            ) => Ok(()),
            // Re-evaluate on the next iteration (authority expiry, command
            // deadline or shutdown deadline); handling never waits for a tick.
            () = &mut deadline_wake => Ok(()),
            _ = &mut shutdown, if loop_state.shutdown_deadline.is_none() => {
                info!(node_id = %runtime.node_id, "HA runtime loop received shutdown signal");
                authority_commands.stop().await;
                if !begin_graceful_handoff(&runtime, &mut loop_state) {
                    exit_cleanup(
                        &runtime, status_tx.as_ref(), &mut loop_state, &mut worker,
                        &mut lifecycle_events, &mut authority_commands, false,
                    ).await;
                    break;
                }
                Ok(())
            }
            event = recv_lifecycle(&mut lifecycle_events) => {
                handle_lifecycle_recv(
                    &runtime, status_tx.as_ref(), &mut loop_state, &worker,
                    &mut lifecycle_events, event,
                );
                Ok(())
            }
            () = &mut verify_wake => {
                handle_verification(&runtime, status_tx.as_ref(), &mut loop_state, &worker);
                Ok(())
            }
            Some(report) = worker.recv_report() => {
                handle_resource_report(&runtime, status_tx.as_ref(), &mut loop_state, &worker, report);
                Ok(())
            }
            recv = io.recv(&mut buffer) => handle_receive_event(
                &runtime, status_tx.as_ref(), &mut loop_state, recv, &buffer,
            ),
            () = &mut advert_sleep => handle_send_event(
                &io, &runtime, peer_addr, status_tx.as_ref(), &mut loop_state,
            ).await,
            () = &mut reconcile_sleep => {
                handle_reconcile_tick(&runtime, &mut loop_state, &worker);
                Ok(())
            }
        };
        if let Err(error) = event {
            exit_cleanup(
                &runtime,
                status_tx.as_ref(),
                &mut loop_state,
                &mut worker,
                &mut lifecycle_events,
                &mut authority_commands,
                true,
            )
            .await;
            return Err(error);
        }
    }

    Ok(())
}

/// Apply queued resource reports, re-evaluate election and authority, start
/// due authority commands and publish, all before the next wait.
fn evaluate_iteration(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &mut ResourceWorkerHandle,
    authority_commands: &mut authority_commands::AuthorityCommands,
) {
    drain_reports(runtime, status_tx, loop_state, worker);
    refresh_local_state(runtime, loop_state, worker);
    authority_commands.start_due(
        runtime,
        &mut loop_state.ownership,
        loop_state.peer.last_fresh_at(),
    );
    publish_loop_status(
        runtime,
        status_tx,
        loop_state,
        tokio::time::Instant::now().into_std(),
    );
}

/// Stop authority commands, then run the bounded exit cleanup.
async fn exit_cleanup(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &mut ResourceWorkerHandle,
    lifecycle_events: &mut LifecycleEvents,
    authority_commands: &mut authority_commands::AuthorityCommands,
    faulted: bool,
) {
    authority_commands.stop().await;
    cleanup_before_exit(
        runtime,
        status_tx,
        loop_state,
        worker,
        lifecycle_events,
        faulted,
    )
    .await;
}

/// B1 cadence (errata N-01): the due-submit time `next_verify_at` (absent
/// while a Verify is in flight: coalesced) and the staleness deadline
/// `next_verify_at + D`, which fires even if no completion ever arrives.
fn verification_wake(ownership: &OwnershipController) -> Option<tokio::time::Instant> {
    ownership
        .next_verify_at()
        .into_iter()
        .chain(ownership.verification_deadline())
        .min()
}

fn handle_reconcile_tick(
    runtime: &HaRuntimeConfig,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
) {
    loop_state.next_reconcile_at = tokio::time::Instant::now() + runtime.dead_timeout();
    if loop_state.ownership.is_startup_reconciled() {
        bind_lifecycle(runtime, loop_state);
        loop_state.ownership.request_reconcile(worker);
    }
}

/// The earliest pure wake: the authority deadline (so a lapsed consent is
/// handled on time instead of at the next advert tick, invariant I3), the
/// external command schedule and the graceful-shutdown deadline.
fn next_deadline(
    runtime: &HaRuntimeConfig,
    loop_state: &HaLoopState,
    authority_commands: &authority_commands::AuthorityCommands,
) -> Option<tokio::time::Instant> {
    [
        loop_state.ownership.authority().expiry(),
        loop_state.shutdown_deadline,
        authority_commands.deadline(runtime, &loop_state.ownership),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn handle_lifecycle_recv(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
    lifecycle_events: &mut LifecycleEvents,
    event: Option<LifecycleEvent>,
) {
    match event {
        Some(event) => handle_lifecycle_event(runtime, status_tx, loop_state, worker, event),
        // Executor gone (generation exhaustion): later submissions fail closed.
        None => *lifecycle_events = None,
    }
}

async fn sleep_until_optional(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

async fn recv_lifecycle(events: &mut LifecycleEvents) -> Option<LifecycleEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// The single lifecycle cleanup deadline (`withdraw_timeout_ms`) must contain
/// the resource worker's fixed withdrawal join bound, which runs in parallel
/// with deactivation and cannot be shortened per deployment.
const fn lifecycle_cleanup_fits(withdraw_timeout_ms: u64) -> bool {
    WITHDRAW_TIMEOUT_MS <= withdraw_timeout_ms
}

/// Start the lifecycle executor when configured. It fails closed when the
/// packaged identities are unusable, and so does the runtime.
fn spawn_lifecycle(
    runtime: &HaRuntimeConfig,
    boot_nonce: super::protocol::Nonce,
    runner: &LifecycleRunner,
) -> Result<Option<(LifecycleExecutor, mpsc::UnboundedReceiver<LifecycleEvent>)>> {
    let Some(lifecycle) = runtime.lifecycle.clone() else {
        return Ok(None);
    };
    if !lifecycle_cleanup_fits(runtime.withdraw_timeout_ms) {
        bail!(
            "ha.withdraw_timeout_ms ({}) must cover the resource worker's fixed {WITHDRAW_TIMEOUT_MS} ms withdrawal bound when ha.lifecycle is configured",
            runtime.withdraw_timeout_ms
        );
    }
    let (hooks, node_id, group_id) = (
        runtime.hooks.clone(),
        runtime.node_id.clone(),
        runtime.group_id.clone(),
    );
    Ok(Some(match runner {
        LifecycleRunner::System => {
            LifecycleExecutor::spawn(lifecycle, hooks, node_id, group_id, boot_nonce)?
        }
        #[cfg(test)]
        LifecycleRunner::Process => {
            LifecycleExecutor::spawn_for_unit_tests(lifecycle, hooks, node_id, group_id, boot_nonce)
        }
        #[cfg(test)]
        LifecycleRunner::Scripted(script) => LifecycleExecutor::spawn_scripted(
            lifecycle,
            hooks,
            node_id,
            group_id,
            boot_nonce,
            script.clone(),
        ),
    }))
}

/// N-03: a Deactivate submitted by a revocation at `revoked_at` must finish,
/// including the executor's termination (SIGTERM grace, SIGKILL, bounded pipe
/// drain: two grace periods), inside the one `withdraw_timeout_ms` deadline
/// that also bounds VIP withdrawal. Notifications never extend it.
fn deactivate_deadline(
    runtime: &HaRuntimeConfig,
    revoked_at: tokio::time::Instant,
) -> tokio::time::Instant {
    revoked_at
        + Duration::from_millis(runtime.withdraw_timeout_ms)
            .saturating_sub(Duration::from_millis(2 * CANCEL_GRACE_MS))
}

/// Bind the next actuator jobs to the current transfer and to a cleanup
/// deadline anchored now. Called before every controller entry point that may
/// revoke or activate, so a job submitted there carries this iteration's facts.
fn bind_lifecycle(runtime: &HaRuntimeConfig, loop_state: &mut HaLoopState) {
    if runtime.lifecycle.is_none() {
        return;
    }
    let (transfer_id, peer_nonce, activation, deactivation) =
        loop_state.handoff.lifecycle_binding();
    loop_state
        .ownership
        .set_lifecycle_binding(LifecycleBinding {
            transfer_id,
            peer_nonce,
            activation,
            deactivation,
            deactivate_deadline: Some(deactivate_deadline(runtime, tokio::time::Instant::now())),
        });
}

fn log_lifecycle_event(runtime: &HaRuntimeConfig, event: &LifecycleEvent) {
    if matches!(event.kind, LifecycleKind::Notify(_)) {
        // The executor already logs failed notifications; they stay log-only.
        return;
    }
    if event.outcome == LifecycleOutcome::Succeeded {
        info!(
            node_id = %runtime.node_id,
            kind = ?event.kind,
            generation = event.generation,
            elapsed_ms = saturating_duration_ms(event.elapsed),
            "HA lifecycle operation succeeded"
        );
    } else {
        warn!(
            node_id = %runtime.node_id,
            kind = ?event.kind,
            generation = event.generation,
            outcome = ?event.outcome,
            elapsed_ms = saturating_duration_ms(event.elapsed),
            exit_code_or_signal = ?event.exit_code_or_signal,
            stderr = %event.stderr_tail,
            "HA lifecycle operation did not succeed"
        );
    }
}

fn saturating_duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Apply one executor completion without awaiting anything, then publish.
fn handle_lifecycle_event(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
    event: LifecycleEvent,
) {
    let now = tokio::time::Instant::now().into_std();
    log_lifecycle_event(runtime, &event);
    bind_lifecycle(runtime, loop_state);
    let effect = loop_state.ownership.apply_lifecycle_event(event, worker);
    apply_authority_effect(runtime, loop_state, effect, now);
    loop_state.handoff.advance(
        &loop_state.ownership,
        loop_state.peer.last_sent_seq().saturating_add(1),
    );
    publish_loop_status(runtime, status_tx, loop_state, now);
}

/// B1 wake: a missed freshness deadline (`next_verify_at + D` without a fresh
/// success) runs the compensation path in this same iteration, before
/// anything else; otherwise a due `Verify(g)` is submitted (Ready and live
/// authority only; coalesced while one is in flight).
fn handle_verification(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
) {
    let now = tokio::time::Instant::now().into_std();
    bind_lifecycle(runtime, loop_state);
    // `request_verify` checks the staleness deadline before it may submit.
    let effect = loop_state.ownership.request_verify(worker);
    apply_authority_effect(runtime, loop_state, effect, now);
    loop_state.handoff.advance(
        &loop_state.ownership,
        loop_state.peer.last_sent_seq().saturating_add(1),
    );
    publish_loop_status(runtime, status_tx, loop_state, now);
}

fn begin_graceful_handoff(runtime: &HaRuntimeConfig, loop_state: &mut HaLoopState) -> bool {
    if !loop_state.ownership.authority().is_valid()
        || loop_state.peer.last_fresh_at().is_none_or(|seen| {
            seen + runtime.dead_timeout() <= tokio::time::Instant::now().into_std()
        })
    {
        // No fresh peer can participate in a cooperative transfer. The
        // ordinary bounded shutdown still revokes, drains and withdraws.
        return false;
    }
    loop_state
        .handoff
        .release(loop_state.peer.last_sent_seq().saturating_add(1));
    loop_state.shutdown_deadline =
        Some(tokio::time::Instant::now() + Duration::from_millis(runtime.withdraw_timeout_ms));
    true
}

/// Spawn the single resource worker and build the initial loop state.
fn spawn_worker(
    runtime: &HaRuntimeConfig,
    command_runner: CommandRunner,
    status_tx: Option<&watch::Sender<HaStatus>>,
    peer: PeerTracker,
    lifecycle: Option<LifecycleExecutor>,
) -> (ResourceWorkerHandle, HaLoopState) {
    let worker = ResourceWorkerHandle::spawn_with_runner(address_manager(runtime), command_runner);
    let loop_state = initialize_loop_state(runtime, status_tx, &worker, peer, lifecycle);
    (worker, loop_state)
}

/// Apply every resource report that is already queued on the worker.
fn drain_reports(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &mut ResourceWorkerHandle,
) {
    while let Some(report) = worker.try_recv_report() {
        handle_resource_report(runtime, status_tx, loop_state, worker, report);
    }
}

/// Resolve and open the configured transport.
///
/// # Errors
///
/// Returns an error if the UDP socket cannot be bound or the peer address
/// does not resolve.
async fn open_transport(
    runtime: &HaRuntimeConfig,
    transport: Transport,
) -> Result<(DatagramIo, String, std::net::SocketAddr)> {
    match transport {
        Transport::Udp => {
            let (socket, local_bind) = bind_ha_socket(&runtime.bind)?;
            let peer_addr = resolve_peer(&runtime.peer).await?;
            Ok((DatagramIo::Udp(socket), local_bind, peer_addr))
        }
        #[cfg(test)]
        Transport::InMemory(endpoint) => {
            let local_bind = format_socket_addr(endpoint.local_addr());
            let peer_addr = endpoint.peer_addr();
            Ok((DatagramIo::InMemory(endpoint), local_bind, peer_addr))
        }
    }
}

fn next_reconcile_deadline(
    runtime: &HaRuntimeConfig,
    reason: HaDecisionReason,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    if reason == HaDecisionReason::StartupHold || reason == HaDecisionReason::Startup {
        now + runtime.dead_timeout() + runtime.hold_down()
    } else {
        now + runtime.dead_timeout()
    }
}

/// Resolve the `ha.peer` host once at startup. The source-IP check pins every
/// received datagram to this address for the lifetime of the process.
async fn resolve_peer(peer: &str) -> Result<std::net::SocketAddr> {
    tokio::net::lookup_host(peer)
        .await
        .with_context(|| format!("invalid HA peer address {peer}"))?
        .next()
        .ok_or_else(|| anyhow!("HA peer address {peer} did not resolve"))
}

fn initialize_loop_state(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    worker: &ResourceWorkerHandle,
    peer: PeerTracker,
    lifecycle: Option<LifecycleExecutor>,
) -> HaLoopState {
    let now_tokio = tokio::time::Instant::now();
    let startup_at = now_tokio.into_std();
    let management = if runtime.addresses.is_empty() {
        ResourceManagement::Unmanaged
    } else {
        ResourceManagement::Managed
    };
    let mut ownership = OwnershipController::new(management);
    ownership.set_failure_policy(runtime.failure_policy.clone());
    ownership.configure_watchdog(runtime, WatchdogDevice::default());
    if let (Some(executor), Some(config)) = (lifecycle, &runtime.lifecycle) {
        // Baseline deactivation precedes reconciliation and any election.
        ownership.configure_lifecycle(executor, config.timeout_ms, worker);
    }
    ownership.start_reconciliation(worker);

    let loop_state = HaLoopState {
        startup_at,
        state: HaState::Init,
        active_peer_conflict: false,
        decision_reason: HaDecisionReason::Startup,
        last_transition_reason: None,
        last_transition_at: None,
        peer_observation: None,
        peer,
        counters: HaCounters::default(),
        sent_adverts: VecDeque::new(),
        next_advert_at: now_tokio + runtime.next_advert_delay(1),
        next_reconcile_at: next_reconcile_deadline(runtime, HaDecisionReason::Startup, now_tokio),
        ownership,
        last_transition_trigger: None,
        handoff: handoff::Handoff::default(),
        shutdown_deadline: None,
        last_refresh_at: now_tokio,
        stall_resync: None,
    };

    publish_loop_status(runtime, status_tx, &loop_state, startup_at);

    loop_state
}

fn log_runtime_start(runtime: &HaRuntimeConfig, local_bind: &str, peer_addr: std::net::SocketAddr) {
    if matches!(runtime.failure_policy, FailurePolicy::Available) {
        warn!(
            node_id = %runtime.node_id,
            "failure_policy available can dual-own during partitions; assumed authority is not split-brain safe"
        );
    }
    info!(
        node_id = %runtime.node_id,
        bind = %local_bind,
        peer = %peer_addr,
        group_id = %runtime.group_id,
        "HA runtime loop started"
    );
}

fn refresh_local_state(
    runtime: &HaRuntimeConfig,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
) {
    let now_tokio = tokio::time::Instant::now();
    let now = now_tokio.into_std();
    loop_state.handoff.advance(
        &loop_state.ownership,
        loop_state.peer.last_sent_seq().saturating_add(1),
    );
    bind_lifecycle(runtime, loop_state);
    let decision = desired_state(
        runtime,
        loop_state.state,
        loop_state.peer_observation.as_ref(),
        loop_state.startup_at,
        now,
        loop_state.active_peer_conflict,
    );
    let mut decision = loop_state.handoff.decision(
        runtime,
        loop_state.peer_observation.as_ref(),
        now,
        loop_state.ownership.authority().is_valid(),
        decision,
    );
    override_decision(runtime, loop_state, now_tokio, &mut decision);
    let bootstrap_ok = bootstrap_rule_satisfied(runtime, loop_state, now);
    loop_state.ownership.set_bootstrap_allowed(bootstrap_ok);
    loop_state.ownership.set_handoff_gate(
        loop_state.handoff.allowed(),
        loop_state.handoff.blocked() || loop_state.shutdown_deadline.is_some(),
    );
    if decision.reason == HaDecisionReason::PeerBecameActiveConflict {
        loop_state.active_peer_conflict = true;
    } else if !has_live_active_peer(runtime, loop_state.peer_observation.as_ref(), now) {
        loop_state.active_peer_conflict = false;
    }
    loop_state.decision_reason = decision.reason;

    if loop_state.state == decision.state {
        debug!(
            node_id = %runtime.node_id,
            state = ?loop_state.state,
            reason = %decision.reason,
            "HA state decision evaluated"
        );
    } else {
        let previous_state = loop_state.state;
        let effect =
            loop_state
                .ownership
                .apply_role_decision(decision.state, decision.reason, worker);
        loop_state.next_reconcile_at = next_reconcile_deadline(runtime, decision.reason, now_tokio);
        dispatch_role_hook(
            runtime,
            &loop_state.ownership,
            effect,
            loop_state.peer_observation.as_ref(),
            now,
        );
        log_state_change(
            previous_state,
            decision.state,
            decision.reason,
            &runtime.node_id,
            runtime.priority,
            loop_state.peer_observation.as_ref(),
            now,
        );
        loop_state.state = decision.state;
        loop_state.last_transition_reason = Some(decision.reason);
        loop_state.last_transition_at = Some(now);
        loop_state.last_transition_trigger = Some(TransitionTrigger::Election);
    }

    // Policy layer: feed the bootstrap verdict and re-evaluate authority for
    // the current role. A grant starts the acquisition; a lapsed strict
    // consent revokes authority and withdraws (invariants I1, I3).
    let authority_effect = loop_state.ownership.refresh_authority(worker);
    apply_authority_effect(runtime, loop_state, authority_effect, now);

    // Unmanaged transitions confirm synchronously. Publish their transfer
    // phase in this iteration, before an advert could expose an absent,
    // unauthorized bootstrap candidate without its release state.
    loop_state.handoff.advance(
        &loop_state.ownership,
        loop_state.peer.last_sent_seq().saturating_add(1),
    );
}

/// Notify and record an authority-layer transition: a policy grant, a lapse,
/// or a lifecycle compensation (activation/verification failure).
fn apply_authority_effect(
    runtime: &HaRuntimeConfig,
    loop_state: &mut HaLoopState,
    effect: RoleTransitionEffect,
    now: Instant,
) {
    if matches!(
        effect,
        RoleTransitionEffect::None | RoleTransitionEffect::AwaitingConfirmation
    ) {
        return;
    }
    dispatch_role_hook(
        runtime,
        &loop_state.ownership,
        effect,
        loop_state.peer_observation.as_ref(),
        now,
    );
    if let RoleTransitionEffect::Demote { reason, .. } = effect {
        loop_state.handoff.fault();
        // Authority transitions are policy-layer facts: the trigger says
        // consent (not the election) drove this transition.
        loop_state.last_transition_reason = Some(reason);
        loop_state.last_transition_at = Some(now);
        loop_state.last_transition_trigger = Some(if reason == HaDecisionReason::RuntimeFault {
            TransitionTrigger::Fault
        } else if runtime.failure_policy == FailurePolicy::External {
            TransitionTrigger::External
        } else {
            TransitionTrigger::Consent
        });
        info!(
            node_id = %runtime.node_id,
            reason = %reason,
            "HA authority revoked by the failure policy; withdrawing"
        );
    }
}

/// Coordinator overrides of the elected role for this evaluation: the
/// post-stall standby hold (started, kept or ended here) and graceful
/// shutdown. Also stamps the evaluation time used for stall detection.
fn override_decision(
    runtime: &HaRuntimeConfig,
    loop_state: &mut HaLoopState,
    now: tokio::time::Instant,
    decision: &mut HaDecision,
) {
    if stalled_owner(runtime, loop_state, now) {
        warn!(
            node_id = %runtime.node_id,
            stalled_ms = saturating_elapsed_ms(now.into_std(), loop_state.last_refresh_at.into_std()),
            "HA coordinator stalled past the follower deadline; yielding to a peer that may have promoted"
        );
        loop_state.stall_resync = Some(StallResync {
            seq: loop_state.peer.last_sent_seq().saturating_add(1),
            until: now + runtime.dead_timeout() + runtime.hold_down(),
        });
    }
    if loop_state
        .stall_resync
        .is_some_and(|resync| now >= resync.until)
    {
        loop_state.stall_resync = None;
    }
    if loop_state.stall_resync.is_some() {
        decision.state = HaState::Standby;
        decision.reason = HaDecisionReason::LocalStall;
    }
    if loop_state.shutdown_deadline.is_some() {
        decision.state = HaState::Standby;
        decision.reason = HaDecisionReason::GracefulShutdown;
    }
    loop_state.last_refresh_at = now;
}

/// The coordinator has not evaluated its policy for longer than the peer's
/// follower deadline while owning under `available` (a stopped or frozen
/// process): the peer may have promoted on its timeout meanwhile, so this
/// claim is stale. The visible incumbent keeps the resource and exactly the
/// stalled side yields. A transfer in progress blocks the peer's timeout
/// fallback (I8), so only an idle handoff can have been displaced. `strict`
/// and `external` peers never promote on silence and keep their own rules.
fn stalled_owner(
    runtime: &HaRuntimeConfig,
    loop_state: &HaLoopState,
    now: tokio::time::Instant,
) -> bool {
    runtime.failure_policy == FailurePolicy::Available
        && loop_state.state == HaState::Active
        && loop_state.handoff.idle()
        && now.saturating_duration_since(loop_state.last_refresh_at)
            >= runtime.dead_timeout() + runtime.hold_down()
}

/// Lifecycle deployments serialize notifications behind critical jobs in the
/// executor, at the notify identity. Without lifecycle the detached hook path
/// is unchanged.
fn notify(
    runtime: &HaRuntimeConfig,
    ownership: &OwnershipController,
    event: HookEvent,
    context: HookContext,
) {
    if ownership.application().configured() {
        ownership.submit_notification(event, context);
    } else {
        spawn_hook(runtime.hooks.clone(), event, context);
    }
}

fn dispatch_role_hook(
    runtime: &HaRuntimeConfig,
    ownership: &OwnershipController,
    effect: RoleTransitionEffect,
    peer_observation: Option<&PeerObservation>,
    now: Instant,
) {
    match effect {
        RoleTransitionEffect::PromoteNow {
            previous_state,
            reason,
        } => {
            let context = hook_context(
                runtime,
                HaState::Active,
                previous_state,
                Some(reason),
                peer_observation,
                now,
            );
            notify(runtime, ownership, HookEvent::Promote, context);
        }
        RoleTransitionEffect::Demote {
            previous_state,
            current_state,
            reason,
        } => {
            let context = hook_context(
                runtime,
                current_state,
                previous_state,
                Some(reason),
                peer_observation,
                now,
            );
            notify(runtime, ownership, HookEvent::Demote, context);
        }
        RoleTransitionEffect::Backup {
            previous_state,
            reason,
        } => {
            let context = hook_context(
                runtime,
                HaState::Standby,
                previous_state,
                Some(reason),
                peer_observation,
                now,
            );
            notify(runtime, ownership, HookEvent::Backup, context);
        }
        RoleTransitionEffect::None | RoleTransitionEffect::AwaitingConfirmation => {}
    }
}

fn handle_resource_report(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
    report: ResourceReport,
) {
    let now_tokio = tokio::time::Instant::now();
    let now = now_tokio.into_std();
    loop_state.next_reconcile_at =
        next_reconcile_deadline(runtime, loop_state.decision_reason, now_tokio);
    bind_lifecycle(runtime, loop_state);
    let effect = loop_state.ownership.apply_worker_report(report, worker);

    match effect {
        ReportEffect::PromoteConfirmed {
            previous_state,
            reason,
        } => {
            let context = hook_context(
                runtime,
                HaState::Active,
                previous_state,
                Some(reason),
                loop_state.peer_observation.as_ref(),
                now,
            );
            notify(runtime, &loop_state.ownership, HookEvent::Promote, context);
        }
        ReportEffect::Fault {
            state,
            previous_state,
        } => {
            loop_state.handoff.fault();
            let context = hook_context(
                runtime,
                state,
                previous_state,
                Some(HaDecisionReason::AddressActionFailed),
                loop_state.peer_observation.as_ref(),
                now,
            );
            notify(runtime, &loop_state.ownership, HookEvent::Fault, context);
        }
        ReportEffect::None => {}
    }

    publish_loop_status(runtime, status_tx, loop_state, now);
}

fn handle_receive_event(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    recv: std::io::Result<(usize, std::net::SocketAddr)>,
    buffer: &[u8],
) -> Result<()> {
    let (len, from) = match recv {
        Ok(value) => value,
        // On loopback (and some real setups) sending an advertisement to a peer
        // whose port has no listener queues an ICMP port-unreachable that the
        // kernel then reports as ECONNREFUSED on the socket's *next* operation —
        // which may be this `recv_from` rather than the send. Treat the same set
        // of transient network errors as non-fatal here as on the send path: a
        // temporarily-down or ICMP-rejecting peer must not tear down the node.
        Err(error) if is_transient_io_error(&error) => {
            warn!(
                %error,
                node_id = %runtime.node_id,
                "HA receive failed transiently; continuing"
            );
            return Ok(());
        }
        Err(error) => {
            return Err(anyhow::Error::new(error)).context("failed to receive HA packet");
        }
    };
    let observed_at = tokio::time::Instant::now().into_std();
    let payload = buffer
        .get(..len)
        .ok_or_else(|| anyhow!("received invalid HA payload length"))?;

    match loop_state.peer.receive(payload, from, observed_at) {
        Ok(admission) => {
            loop_state.counters.packets_received =
                loop_state.counters.packets_received.saturating_add(1);
            match admission {
                Admission::Fresh(packet) => {
                    if loop_state
                        .stall_resync
                        .is_some_and(|resync| packet.ack_seq >= resync.seq)
                    {
                        // The peer has seen our post-stall standby advert:
                        // its state is current, so election resumes.
                        loop_state.stall_resync = None;
                    }
                    loop_state
                        .ownership
                        .set_fencing(crate::gruezi::ownership::FencingState::None);
                    loop_state.handoff.observe(
                        runtime,
                        &packet,
                        loop_state
                            .peer_observation
                            .as_ref()
                            .map(|peer| peer.observed_at),
                        observed_at,
                        &loop_state.ownership,
                    );
                    observe_peer(&mut loop_state.peer_observation, &packet, observed_at);
                    update_consent(runtime, loop_state, &packet);
                    debug!(
                        node_id = %runtime.node_id,
                        peer = %from,
                        peer_state = ?packet.role,
                        peer_priority = packet.priority,
                        seq = packet.seq,
                        "received fresh HA advertisement"
                    );
                }
                Admission::Unconfirmed { candidate: true } => {
                    info!(
                        node_id = %runtime.node_id,
                        peer = %from,
                        "recorded an HA peer nonce as establishment candidate; waiting for it to echo our nonce"
                    );
                }
                Admission::Unconfirmed { candidate: false } | Admission::Reordered => {
                    debug!(
                        node_id = %runtime.node_id,
                        peer = %from,
                        ?admission,
                        "HA advertisement accepted without refreshing liveness"
                    );
                }
            }
        }
        Err((cause, count)) => {
            record_rejection(runtime, &mut loop_state.counters, cause, count, from);
        }
    }

    publish_loop_status(runtime, status_tx, loop_state, observed_at);
    Ok(())
}

/// Feed one fresh, validated advert to election.
fn observe_peer(
    peer_observation: &mut Option<PeerObservation>,
    packet: &HaPacket,
    observed_at: Instant,
) {
    let previous_peer_state = peer_observation.as_ref().and_then(|observation| {
        (observation.node_id == packet.sender_id).then_some(observation.state)
    });
    *peer_observation = Some(PeerObservation {
        node_id: packet.sender_id.clone(),
        state: packet.role,
        priority: packet.priority,
        became_active: matches!(packet.role, HaState::Active)
            && matches!(
                previous_peer_state,
                Some(previous_state) if previous_state != HaState::Active
            ),
        observed_at,
        resource_state: packet.resource_state,
        authority_source: packet.authority_source,
    });
}

/// Advance bilateral consent from one fresh authenticated advert (design spec
/// 4.4): `ack_seq` resolves against our own send history, and the deadline is
/// anchored at the SEND time of the newest acknowledged own sequence number.
/// A fresh advert that acknowledges none of our sends clears consent: a new
/// session never inherits old authority.
fn update_consent(runtime: &HaRuntimeConfig, loop_state: &mut HaLoopState, packet: &HaPacket) {
    let acked_send_at = loop_state
        .sent_adverts
        .iter()
        .rev()
        .find(|(seq, _)| *seq <= packet.ack_seq)
        .map(|(_, sent_at)| *sent_at);
    loop_state
        .ownership
        .observe_consent(acked_send_at, runtime.dead_timeout());
}

/// BOOTSTRAP rule (design spec 3.1): a fresh session, both nodes reporting
/// the resource absent/unmanaged, the peer holding no authority, and this
/// node winning the rank via `outranks`. Priority alone never authorizes.
fn bootstrap_rule_satisfied(
    runtime: &HaRuntimeConfig,
    loop_state: &HaLoopState,
    now: Instant,
) -> bool {
    if !loop_state.handoff.bootstrap_permitted() {
        return false;
    }
    let Some(peer) = loop_state.peer_observation.as_ref() else {
        return false;
    };
    if runtime.follower_deadline(peer.observed_at) <= now {
        return false;
    }
    if loop_state
        .peer
        .last_fresh_at()
        .is_none_or(|seen| runtime.follower_deadline(seen) <= now)
    {
        return false;
    }
    if !loop_state
        .ownership
        .resource_state()
        .is_confirmed_absent_or_unmanaged()
    {
        return false;
    }
    // VIP absence alone is not passivity: with lifecycle configured the local
    // application must be confirmed Inactive for the current generation. The
    // peer's side is covered by its conservative advert (releasing/uncertain
    // until its own cleanup is resolved).
    if !loop_state
        .ownership
        .application()
        .passive_confirmed(loop_state.ownership.generation())
    {
        return false;
    }
    if !matches!(
        peer.resource_state,
        AdvertResourceState::Absent | AdvertResourceState::Unmanaged
    ) {
        return false;
    }
    if peer.authority_source != AdvertAuthoritySource::None {
        return false;
    }
    outranks(
        runtime.priority,
        &runtime.node_id,
        peer.priority,
        &peer.node_id,
    )
}

/// Count a rejected datagram. Rejections change counters and nothing else.
fn record_rejection(
    runtime: &HaRuntimeConfig,
    counters: &mut HaCounters,
    cause: PacketRejection,
    count: u64,
    from: std::net::SocketAddr,
) {
    counters.invalid_packets = counters.invalid_packets.saturating_add(1);
    match cause {
        PacketRejection::BadTag => {
            counters.auth_failures = counters.auth_failures.saturating_add(1);
        }
        PacketRejection::DuplicateNodeId => {
            counters.duplicate_node_id_packets =
                counters.duplicate_node_id_packets.saturating_add(1);
        }
        _ => {}
    }
    // Warn on the 1st, 2nd, 4th, 8th... rejection per cause so a flood of
    // spoofed datagrams cannot flood the log; the counters stay exact.
    if count.is_power_of_two() {
        warn!(node_id = %runtime.node_id, from = %from, %cause, count, "rejected HA packet");
    } else {
        debug!(node_id = %runtime.node_id, from = %from, %cause, count, "rejected HA packet");
    }
}

async fn handle_send_event(
    io: &DatagramIo,
    runtime: &HaRuntimeConfig,
    peer_addr: std::net::SocketAddr,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
) -> Result<()> {
    if stalled_owner(runtime, loop_state, tokio::time::Instant::now()) {
        // A thawed owner must evaluate the stall before it advertises: its
        // stale Active claim would make the incumbent yield too. The advert
        // stays due and is sent right after the next policy evaluation.
        return Ok(());
    }
    let mut advert = advert_state(&loop_state.ownership);
    (advert.transfer_id, advert.transfer_phase) = loop_state.handoff.advert();
    debug!(
        node_id = %runtime.node_id,
        transfer_id = advert.transfer_id,
        phase = ?advert.transfer_phase,
        resource = ?advert.resource_state,
        "periodic HA handoff state"
    );
    // Sequence exhaustion is fatal on purpose: the runtime exits and restarts
    // with a new nonce instead of ever reusing a sequence number.
    let (seq, encoded) = loop_state.peer.next_advert(advert)?;
    record_sent_advert(loop_state, seq);
    match send_advertisement(io, runtime, peer_addr, &encoded, seq, advert.role).await {
        Ok(()) => {
            loop_state.counters.packets_sent = loop_state.counters.packets_sent.saturating_add(1);
        }
        Err(error) if is_transient_send_failure(&error) => {
            warn!(
                %error,
                node_id = %runtime.node_id,
                peer = %peer_addr,
                state = ?advert.role,
                "HA advertisement send failed; continuing"
            );
        }
        Err(error) => return Err(error),
    }

    loop_state.next_advert_at =
        tokio::time::Instant::now() + runtime.next_advert_delay(seq.saturating_add(1));
    publish_loop_status(
        runtime,
        status_tx,
        loop_state,
        tokio::time::Instant::now().into_std(),
    );
    Ok(())
}

/// Remember when a given own sequence number was handed to the transport
/// (design spec 4.4: consent is anchored at these send times).
fn record_sent_advert(loop_state: &mut HaLoopState, seq: u64) {
    loop_state
        .sent_adverts
        .push_back((seq, tokio::time::Instant::now()));
    while loop_state.sent_adverts.len() > SENT_ADVERT_HISTORY {
        if loop_state.sent_adverts.pop_front().is_none() {
            break;
        }
    }
}

fn hook_context(
    runtime: &HaRuntimeConfig,
    state: HaState,
    previous_state: HaState,
    reason: Option<HaDecisionReason>,
    peer_observation: Option<&PeerObservation>,
    now: Instant,
) -> HookContext {
    HookContext {
        node_id: runtime.node_id.clone(),
        group_id: runtime.group_id.clone(),
        interface: runtime.interface.clone(),
        state,
        previous_state,
        reason,
        priority: runtime.priority,
        peer_id: peer_observation.map(|peer| peer.node_id.clone()),
        peer_state: peer_observation.map(|peer| peer.state),
        peer_priority: peer_observation.map(|peer| peer.priority),
        last_peer_seen_ms_ago: peer_observation
            .map(|peer| saturating_elapsed_ms(now, peer.observed_at)),
    }
}

fn address_manager(runtime: &HaRuntimeConfig) -> AddressManager {
    AddressManager {
        ip_command: runtime.ip_command.clone(),
        arping_command: runtime.arping_command.clone(),
        ndsend_command: runtime.ndsend_command.clone(),
        interface: runtime.interface.clone(),
        addresses: runtime.addresses.clone(),
    }
}

async fn cleanup_before_exit(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &mut ResourceWorkerHandle,
    lifecycle_events: &mut LifecycleEvents,
    faulted: bool,
) {
    cleanup_before_exit_with_hook(
        runtime,
        status_tx,
        loop_state,
        worker,
        lifecycle_events,
        faulted,
        |context| runtime.hooks.run(HookEvent::Fault, context),
    )
    .await;
}

async fn cleanup_before_exit_with_hook<H, F>(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &mut ResourceWorkerHandle,
    lifecycle_events: &mut LifecycleEvents,
    faulted: bool,
    fault_hook: H,
) where
    H: FnOnce(HookContext) -> F,
    F: Future<Output = Result<()>>,
{
    let started = tokio::time::Instant::now();
    let cleanup_deadline = started + Duration::from_millis(WITHDRAW_TIMEOUT_MS);
    // With lifecycle, `withdraw_timeout_ms` is ONE absolute deadline for
    // script cancellation, VIP withdrawal and deactivation (task 5e); the
    // startup check guarantees it contains the worker's fixed join bound.
    let lifecycle = loop_state.ownership.application().configured();
    let lifecycle_deadline = started + Duration::from_millis(runtime.withdraw_timeout_ms);
    bind_lifecycle(runtime, loop_state);
    let previous_state = loop_state.state;
    let now = tokio::time::Instant::now().into_std();
    let reason = if faulted {
        HaDecisionReason::RuntimeFault
    } else {
        HaDecisionReason::GracefulShutdown
    };

    // Runs in parallel with the Deactivate that `shutdown` submits.
    let withdraw_report = loop_state.ownership.shutdown(worker).await;
    let withdrawal_failed = withdraw_report.kind == ResourceReportKind::Fault;
    let mut awaiting_fault = false;
    if withdrawal_failed {
        warn!(
            error = ?withdraw_report.error,
            node_id = %runtime.node_id,
            "HA shutdown address cleanup failed"
        );
    }

    if faulted || withdrawal_failed {
        let fault_reason = if faulted {
            reason
        } else {
            HaDecisionReason::AddressActionFailed
        };
        let context = hook_context(
            runtime,
            HaState::Init,
            previous_state,
            Some(fault_reason),
            loop_state.peer_observation.as_ref(),
            now,
        );
        if lifecycle {
            // Queued behind the critical Deactivate; cancelled at the deadline.
            loop_state
                .ownership
                .submit_notification(HookEvent::Fault, context);
            awaiting_fault = runtime.hooks.on_fault.is_some();
        } else if let Err(error) =
            // Withdrawal owns the budget first. A notification cannot extend it,
            // even when the configured hook timeout exceeds the remaining budget.
            tokio::time::timeout_at(cleanup_deadline, fault_hook(context))
                    .await
                    .context("fault hook exceeded the remaining withdrawal deadline")
                    .and_then(std::convert::identity)
        {
            warn!(%error, node_id = %runtime.node_id, "HA fault hook execution failed during shutdown");
        }
    }
    if lifecycle {
        finish_lifecycle(
            runtime,
            loop_state,
            worker,
            lifecycle_events,
            lifecycle_deadline,
            awaiting_fault,
        )
        .await;
    }

    if previous_state != HaState::Init {
        log_state_change(
            previous_state,
            HaState::Init,
            reason,
            &runtime.node_id,
            runtime.priority,
            loop_state.peer_observation.as_ref(),
            now,
        );
        loop_state.last_transition_reason = Some(reason);
        loop_state.last_transition_at = Some(now);
        loop_state.last_transition_trigger = Some(if faulted {
            TransitionTrigger::Fault
        } else {
            TransitionTrigger::Shutdown
        });
    }

    loop_state.state = HaState::Init;
    loop_state.decision_reason = reason;
    publish_loop_status(runtime, status_tx, loop_state, now);
}

/// Wait, inside the one lifecycle deadline, for the cleanup Deactivate (and a
/// submitted fault notification) to settle, then cancel and reap whatever is
/// left. Script termination after the deadline is never waited for.
async fn finish_lifecycle(
    runtime: &HaRuntimeConfig,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
    lifecycle_events: &mut LifecycleEvents,
    deadline: tokio::time::Instant,
    mut awaiting_fault: bool,
) {
    if let Some(events) = lifecycle_events.as_mut() {
        while loop_state.ownership.application().operation.is_some() || awaiting_fault {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(event)) => {
                    if event.kind == LifecycleKind::Notify(HookEvent::Fault) {
                        awaiting_fault = false;
                    }
                    log_lifecycle_event(runtime, &event);
                    loop_state.ownership.apply_lifecycle_event(event, worker);
                }
                Ok(None) | Err(_) => break,
            }
        }
    }
    if let Some(executor) = loop_state.ownership.take_lifecycle_executor() {
        match tokio::time::timeout_at(deadline, executor.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(%error, node_id = %runtime.node_id, "HA lifecycle executor failed during shutdown");
            }
            Err(_) => warn!(
                node_id = %runtime.node_id,
                "HA lifecycle work still terminating at the withdrawal deadline; not waiting"
            ),
        }
    }
    // Record what the shutdown itself settled: a cancelled Deactivate never
    // confirmed passivity and must leave a failure, not a clean exit.
    if let Some(events) = lifecycle_events.as_mut() {
        while let Ok(event) = events.try_recv() {
            log_lifecycle_event(runtime, &event);
            loop_state.ownership.apply_lifecycle_event(event, worker);
        }
    }
}

/// Advert fields owned by the coordinator: elected role plus the ownership
/// controller's authority and resource view.
fn advert_state(ownership: &OwnershipController) -> AdvertState {
    let authority = ownership.authority();
    AdvertState {
        role: ownership.advertised_role(),
        authority_source: AdvertAuthoritySource::from_authority(&authority),
        resource_state: advertised_resource_state(ownership),
        term: authority.term(),
        transfer_id: 0,
        transfer_phase: super::protocol::TransferPhase::None,
    }
}

/// Conservative release evidence (task 5c, existing wire values 4 and 5):
/// while application cleanup is unresolved the node advertises `releasing`,
/// and `uncertain` once it failed, even when the VIP is absent or there is no
/// VIP at all. A peer's bootstrap rule and handoff acceptance therefore never
/// see `absent`/`unmanaged` before this node is confirmed passive. The
/// physical VIP observation stays a separate status fact.
fn advertised_resource_state(ownership: &OwnershipController) -> AdvertResourceState {
    let physical = AdvertResourceState::from(ownership.resource_state());
    match ownership.application().phase {
        ApplicationPhase::Uncertain { .. } => AdvertResourceState::Uncertain,
        ApplicationPhase::Deactivating { .. } if physical != AdvertResourceState::Uncertain => {
            AdvertResourceState::Releasing
        }
        _ => physical,
    }
}

async fn send_advertisement(
    io: &DatagramIo,
    runtime: &HaRuntimeConfig,
    peer_addr: std::net::SocketAddr,
    encoded: &[u8],
    seq: u64,
    state: HaState,
) -> Result<()> {
    io.send(encoded, peer_addr)
        .await
        .with_context(|| format!("failed to send HA packet to {peer_addr}"))?;

    debug!(
        node_id = %runtime.node_id,
        state = ?state,
        sequence = seq,
        peer = %peer_addr,
        "sent HA advertisement"
    );

    Ok(())
}

fn is_transient_io_error(io_error: &std::io::Error) -> bool {
    matches!(
        io_error.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::TimedOut
    )
}

fn is_transient_send_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(is_transient_io_error)
}

fn log_state_change(
    previous: HaState,
    current: HaState,
    reason: HaDecisionReason,
    node_id: &str,
    local_priority: u8,
    peer_observation: Option<&PeerObservation>,
    now: Instant,
) {
    if previous != current {
        let peer_node_id = peer_observation.map_or("unknown", |peer| peer.node_id.as_str());
        let peer_state = peer_observation.map(|peer| format!("{:?}", peer.state));
        let peer_priority = peer_observation.map(|peer| peer.priority);
        let last_peer_seen_ms_ago =
            peer_observation.map(|peer| saturating_elapsed_ms(now, peer.observed_at));

        let span = tracing::info_span!(
            "gruezi.ha.state_change",
            node_id,
            ?previous,
            ?current,
            reason = %reason
        );
        let _entered = span.enter();
        info!(
            node_id,
            ?previous,
            ?current,
            reason = %reason,
            local_priority,
            peer_node_id,
            peer_state = peer_state.as_deref().unwrap_or("unknown"),
            peer_priority,
            last_peer_seen_ms_ago,
            "HA state changed"
        );
    }
}

fn bind_ha_socket(bind: &str) -> Result<(UdpSocket, String)> {
    let bind_addr = bind
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid HA bind address {bind}"))?;

    if bind_addr.ip().is_unspecified()
        && bind_addr.is_ipv4()
        && let Ok(socket) = bind_udp_socket_with_retry(
            Domain::IPV6,
            std::net::SocketAddr::from(([0_u16; 8], bind_addr.port())),
            Some(false),
        )
    {
        let display = format_socket_addr(socket.local_addr()?);
        return Ok((socket, display));
    }

    let domain = if bind_addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let only_v6 = bind_addr.is_ipv6().then_some(true);
    let socket = bind_udp_socket_with_retry(domain, bind_addr, only_v6)?;
    let display = format_socket_addr(socket.local_addr()?);

    Ok((socket, display))
}

/// Bind the HA UDP socket, retrying briefly on a transient `AddressInUse`.
///
/// On Linux, binding a loopback UDP port that was very recently released can
/// momentarily fail with `EADDRINUSE` even though `ss` shows no socket holding it
/// (observed under rapid rebind — a quick daemon restart, or parallel test churn).
/// The port is genuinely free, so a few short retries reliably succeed; a port that
/// is actually occupied still fails once the bounded window is exhausted.
fn bind_udp_socket_with_retry(
    domain: Domain,
    socket_addr: std::net::SocketAddr,
    only_v6: Option<bool>,
) -> Result<UdpSocket> {
    const MAX_ATTEMPTS: u32 = 20;
    const BACKOFF: Duration = Duration::from_millis(25);

    let mut attempt: u32 = 0;
    loop {
        match bind_udp_socket(domain, socket_addr, only_v6) {
            Ok(socket) => return Ok(socket),
            Err(error) => {
                attempt += 1;
                let transient = error
                    .chain()
                    .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                    .any(|io| io.kind() == std::io::ErrorKind::AddrInUse);
                if !transient || attempt >= MAX_ATTEMPTS {
                    return Err(error);
                }
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

fn bind_udp_socket(
    domain: Domain,
    socket_addr: std::net::SocketAddr,
    only_v6: Option<bool>,
) -> Result<UdpSocket> {
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    if let Some(only_v6) = only_v6 {
        socket.set_only_v6(only_v6)?;
    }

    socket.bind(&SockAddr::from(socket_addr))?;
    socket.set_nonblocking(true)?;

    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket).map_err(Into::into)
}

fn format_socket_addr(addr: std::net::SocketAddr) -> String {
    if addr.is_ipv6() {
        format!("[{}]:{}", addr.ip(), addr.port())
    } else {
        format!("{}:{}", addr.ip(), addr.port())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HaRuntimeConfig, HaState, handle_receive_event, is_transient_io_error,
        is_transient_send_failure,
    };
    use crate::{
        config::Config,
        gruezi::{
            addresses::{CommandRunner, ResourceWorkerHandle, SimulatedKernel},
            ha::{
                protocol::{
                    AdvertAuthoritySource, AdvertResourceState, HaPacket, Nonce, PacketKey,
                    TransferPhase, ZERO_NONCE,
                    tests::{TEST_KEY, sample_packet},
                },
                session::PeerTracker,
            },
        },
    };
    use anyhow::{Result, anyhow};
    use std::net::SocketAddr;

    const LOCAL_NONCE: Nonce = [0xa5; 16];
    const PEER_NONCE: Nonce = [0xb5; 16];

    #[tokio::test(start_paused = true)]
    async fn hanging_fault_hook_does_not_exceed_join_deadline() -> Result<()> {
        use crate::gruezi::addresses::CommandGate;
        use anyhow::Context;
        use tokio::sync::oneshot;
        let config = Config::from_yaml_str(
            "mode: ha\nnode:\n  id: node-a\nha:\n  failure_policy: available\n  peer_id: node-b\n  auth: {key: shared-secret-for-tests}\n  group_id: cluster-ha\n  interface: eth0\n  peer: 127.0.0.1:9\n  addresses: [10.0.0.10/24]\n",
        )?;
        let runtime = HaRuntimeConfig::try_from(&config)?;
        let kernel = SimulatedKernel::new_shared();
        SimulatedKernel::insert_address(&kernel, "eth0", "10.0.0.10/24", "");
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        {
            let mut guard = kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.gate_next_del = Some(CommandGate {
                entered: entered_tx,
                release: release_rx,
            });
        }
        let mut worker = ResourceWorkerHandle::spawn_with_runner(
            super::address_manager(&runtime),
            CommandRunner::Simulated(kernel.clone()),
        );
        let peer = PeerTracker::new(&runtime, "127.0.0.1".parse()?, LOCAL_NONCE)?;
        let mut loop_state = super::initialize_loop_state(&runtime, None, &worker, peer, None);
        entered_rx
            .await
            .context("withdrawal entered the command gate")?;
        let started = tokio::time::Instant::now();
        let budget =
            std::time::Duration::from_millis(crate::gruezi::addresses::WITHDRAW_TIMEOUT_MS);
        let hook_started = std::cell::Cell::new(false);
        let mut lifecycle_events = None;
        let cleanup = super::cleanup_before_exit_with_hook(
            &runtime,
            None,
            &mut loop_state,
            &mut worker,
            &mut lifecycle_events,
            true,
            |_| {
                hook_started.set(true);
                assert_eq!(
                    SimulatedKernel::table_snapshot(&kernel),
                    Vec::<String>::new()
                );
                std::future::pending()
            },
        );
        let release = async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = release_tx.send(Ok(()));
        };
        tokio::join!(cleanup, release);
        assert!(hook_started.get());
        assert_eq!(tokio::time::Instant::now() - started, budget);
        assert_eq!(
            SimulatedKernel::table_snapshot(&kernel),
            Vec::<String>::new()
        );
        assert!(!loop_state.ownership.owned());
        println!(
            "HANGING FAULT HOOK: real worker drained at 2000ms; total cleanup join=5000ms; hook cancelled; kernel table empty"
        );
        Ok(())
    }

    fn unmanaged_runtime() -> Result<HaRuntimeConfig> {
        let config = Config::from_yaml_str(
            "mode: ha\nnode:\n  id: node-a\nha:\n  failure_policy: available\n  peer_id: node-b\n  auth: {key: shared-secret-for-tests}\n  group_id: cluster-ha\n  peer: 127.0.0.1:9\n  addresses: []\n",
        )?;
        HaRuntimeConfig::try_from(&config)
    }

    fn unmanaged_loop() -> Result<(HaRuntimeConfig, ResourceWorkerHandle, super::HaLoopState)> {
        let runtime = unmanaged_runtime()?;
        let worker = ResourceWorkerHandle::spawn_with_runner(
            super::address_manager(&runtime),
            CommandRunner::Simulated(SimulatedKernel::new_shared()),
        );
        let peer = PeerTracker::new(&runtime, "127.0.0.1".parse()?, LOCAL_NONCE)?;
        let loop_state = super::initialize_loop_state(&runtime, None, &worker, peer, None);
        Ok((runtime, worker, loop_state))
    }

    fn deliver(
        runtime: &HaRuntimeConfig,
        loop_state: &mut super::HaLoopState,
        bytes: &[u8],
        from: SocketAddr,
    ) -> Result<()> {
        handle_receive_event(runtime, None, loop_state, Ok((bytes.len(), from)), bytes)
    }

    fn peer_advert(receiver_nonce: Nonce, seq: u64, key: &[u8]) -> Result<Vec<u8>> {
        HaPacket {
            sender_nonce: PEER_NONCE,
            receiver_nonce,
            seq,
            role: HaState::Active,
            ..sample_packet()
        }
        .encode(&PacketKey::new(key)?)
    }

    #[tokio::test]
    async fn rejections_change_counters_but_never_election_or_liveness() -> Result<()> {
        let (runtime, _worker, mut loop_state) = unmanaged_loop()?;
        let peer: SocketAddr = "127.0.0.1:40000".parse()?;

        // First advert is only an establishment candidate (not fresh):
        // election still has no peer.
        deliver(
            &runtime,
            &mut loop_state,
            &peer_advert(ZERO_NONCE, 1, TEST_KEY)?,
            peer,
        )?;
        assert!(loop_state.peer_observation.is_none());
        // Second advert echoes our nonce: the session is fresh.
        deliver(
            &runtime,
            &mut loop_state,
            &peer_advert(LOCAL_NONCE, 2, TEST_KEY)?,
            peer,
        )?;
        let observed = loop_state
            .peer_observation
            .clone()
            .ok_or_else(|| anyhow!("fresh advert must feed election"))?;
        assert_eq!(observed.state, HaState::Active);
        assert_eq!(loop_state.counters.packets_received, 2);

        let own_id = HaPacket {
            sender_id: "node-a".to_owned(),
            sender_nonce: PEER_NONCE,
            receiver_nonce: LOCAL_NONCE,
            seq: 3,
            ..sample_packet()
        }
        .encode(&PacketKey::new(TEST_KEY)?)?;
        let attacks: [(Vec<u8>, SocketAddr); 5] = [
            (
                peer_advert(LOCAL_NONCE, 3, TEST_KEY)?,
                "127.0.0.9:40000".parse()?,
            ),
            (peer_advert(LOCAL_NONCE, 3, b"wrong-key-for-forgery")?, peer),
            (peer_advert(LOCAL_NONCE, 2, TEST_KEY)?, peer),
            (own_id, peer),
            (vec![0; 8], peer),
        ];
        for (index, (bytes, from)) in attacks.iter().enumerate() {
            let state_before = loop_state.state;
            deliver(&runtime, &mut loop_state, bytes, *from)?;
            assert_eq!(loop_state.peer_observation.as_ref(), Some(&observed));
            assert_eq!(loop_state.state, state_before);
            assert_eq!(loop_state.counters.packets_received, 2);
            assert_eq!(
                loop_state.counters.invalid_packets,
                u64::try_from(index)? + 1
            );
        }
        let rejections = loop_state.peer.rejections();
        assert_eq!(rejections.wrong_source, 1);
        assert_eq!(rejections.bad_tag, 1);
        assert_eq!(rejections.duplicate, 1);
        assert_eq!(rejections.duplicate_node_id, 1);
        assert_eq!(rejections.malformed, 1);
        assert_eq!(loop_state.counters.auth_failures, 1);
        assert_eq!(loop_state.counters.duplicate_node_id_packets, 1);
        Ok(())
    }

    #[tokio::test]
    async fn unmanaged_node_advertises_unmanaged_resource_state() -> Result<()> {
        let (_runtime, _worker, mut loop_state) = unmanaged_loop()?;
        let state = super::advert_state(&loop_state.ownership);
        assert_eq!(state.resource_state, AdvertResourceState::Unmanaged);
        assert_eq!(state.authority_source, AdvertAuthoritySource::None);
        let (seq, bytes) = loop_state.peer.next_advert(state)?;
        let advert = HaPacket::decode(&bytes, &PacketKey::new(TEST_KEY)?)?;
        assert_eq!(seq, 1);
        assert_eq!(advert.seq, 1);
        assert_eq!(advert.sender_nonce, LOCAL_NONCE);
        assert_eq!(advert.receiver_nonce, ZERO_NONCE);
        assert_eq!(advert.ack_seq, 0);
        assert_eq!(advert.resource_state, AdvertResourceState::Unmanaged);
        assert_eq!(advert.transfer_phase, TransferPhase::None);
        assert_eq!(advert.transfer_id, 0);
        Ok(())
    }

    #[test]
    fn classifies_permission_denied_send_failure_as_transient() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied).into();
        assert!(is_transient_send_failure(&error));
    }

    #[test]
    fn does_not_classify_invalid_input_as_transient_send_failure() {
        let error = std::io::Error::from(std::io::ErrorKind::InvalidInput).into();
        assert!(!is_transient_send_failure(&error));
    }

    #[test]
    fn classifies_connection_refused_receive_as_transient() {
        // On loopback, sending to a peer port with no listener makes the kernel
        // report the ICMP port-unreachable as ECONNREFUSED on the socket's next
        // operation (which may be recv_from). This must be tolerated, not fatal,
        // so a node whose peer is down does not tear itself down.
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(is_transient_io_error(&error));
    }

    #[test]
    fn does_not_classify_invalid_input_receive_as_transient() {
        let error = std::io::Error::from(std::io::ErrorKind::InvalidInput);
        assert!(!is_transient_io_error(&error));
    }
}
