use super::{
    config::HaRuntimeConfig,
    election::{HaDecisionReason, HaState, PeerObservation, desired_state, has_live_active_peer},
    protocol::{HaPacket, auth_tag, verify_auth_tag},
    status::{HaStatus, publish_loop_status, saturating_elapsed_ms},
};
use crate::gruezi::{
    addresses::{AddressManager, ResourceReport, ResourceReportKind, ResourceWorkerHandle},
    hooks::{HookContext, HookEvent, spawn_hook},
    ownership::{OwnershipController, ReportEffect, ResourceManagement, RoleTransitionEffect},
};
use anyhow::{Context, Result, anyhow, bail};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::watch};
use tracing::{debug, info, warn};

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
    pub(super) sequence: u64,
    pub(super) state: HaState,
    // Keep this sticky while a live peer remains Active so an asymmetric-loss
    // demotion does not immediately flip back on the next advertisement.
    pub(super) active_peer_conflict: bool,
    pub(super) decision_reason: HaDecisionReason,
    pub(super) last_transition_reason: Option<HaDecisionReason>,
    pub(super) last_transition_at: Option<Instant>,
    pub(super) peer_observation: Option<PeerObservation>,
    pub(super) counters: HaCounters,
    pub(super) next_advert_at: tokio::time::Instant,
    pub(super) next_reconcile_at: tokio::time::Instant,
    pub(super) ownership: OwnershipController,
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
    let (socket, local_bind) = bind_ha_socket(&runtime.bind)?;
    let peer_addr = runtime
        .peer
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid HA peer address {}", runtime.peer))?;
    let mut buffer = [0_u8; 1_024];
    let mut worker = ResourceWorkerHandle::spawn(address_manager(&runtime));
    let mut loop_state = initialize_loop_state(&runtime, status_tx.as_ref(), &worker);
    tokio::pin!(shutdown);

    log_runtime_start(&runtime, &local_bind, peer_addr);

    loop {
        refresh_local_state(&runtime, status_tx.as_ref(), &mut loop_state, &worker);
        while let Some(report) = worker.try_recv_report() {
            handle_resource_report(
                &runtime,
                status_tx.as_ref(),
                &mut loop_state,
                &worker,
                report,
            );
        }

        let advert_sleep = tokio::time::sleep_until(loop_state.next_advert_at);
        let reconcile_sleep = tokio::time::sleep_until(loop_state.next_reconcile_at);
        tokio::pin!(advert_sleep);
        tokio::pin!(reconcile_sleep);

        tokio::select! {
            _ = &mut shutdown => {
                info!(node_id = %runtime.node_id, "HA runtime loop received shutdown signal");
                cleanup_before_exit(
                    &runtime,
                    status_tx.as_ref(),
                    &mut loop_state,
                    &mut worker,
                    false,
                )
                .await;
                break;
            }
            Some(report) = worker.recv_report() => {
                handle_resource_report(
                    &runtime,
                    status_tx.as_ref(),
                    &mut loop_state,
                    &worker,
                    report,
                );
            }
            recv = socket.recv_from(&mut buffer) => {
                if let Err(error) = handle_receive_event(
                    &runtime,
                    status_tx.as_ref(),
                    &mut loop_state,
                    recv,
                    &buffer,
                ) {
                    cleanup_before_exit(
                        &runtime,
                        status_tx.as_ref(),
                        &mut loop_state,
                        &mut worker,
                        true,
                    )
                    .await;
                    return Err(error);
                }
            }
            () = &mut advert_sleep => {
                if let Err(error) = handle_send_event(
                    &socket,
                    &runtime,
                    peer_addr,
                    status_tx.as_ref(),
                    &mut loop_state,
                )
                .await
                {
                    cleanup_before_exit(
                        &runtime,
                        status_tx.as_ref(),
                        &mut loop_state,
                        &mut worker,
                        true,
                    )
                    .await;
                    return Err(error);
                }
            }
            () = &mut reconcile_sleep => {
                loop_state.next_reconcile_at =
                    tokio::time::Instant::now() + runtime.dead_timeout();
                if loop_state.ownership.is_startup_reconciled() {
                    loop_state.ownership.request_reconcile(&worker);
                }
            }
        }
    }

    Ok(())
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

fn initialize_loop_state(
    runtime: &HaRuntimeConfig,
    status_tx: Option<&watch::Sender<HaStatus>>,
    worker: &ResourceWorkerHandle,
) -> HaLoopState {
    let now_tokio = tokio::time::Instant::now();
    let startup_at = now_tokio.into_std();
    let management = if runtime.addresses.is_empty() {
        ResourceManagement::Unmanaged
    } else {
        ResourceManagement::Managed
    };
    let mut ownership = OwnershipController::new(management);
    ownership.start_reconciliation(worker);

    let loop_state = HaLoopState {
        startup_at,
        sequence: 0,
        state: HaState::Init,
        active_peer_conflict: false,
        decision_reason: HaDecisionReason::Startup,
        last_transition_reason: None,
        last_transition_at: None,
        peer_observation: None,
        counters: HaCounters::default(),
        next_advert_at: now_tokio + runtime.next_advert_delay(1),
        next_reconcile_at: next_reconcile_deadline(runtime, HaDecisionReason::Startup, now_tokio),
        ownership,
    };

    publish_loop_status(runtime, status_tx, &loop_state, startup_at);

    loop_state
}

fn log_runtime_start(runtime: &HaRuntimeConfig, local_bind: &str, peer_addr: std::net::SocketAddr) {
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
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
    worker: &ResourceWorkerHandle,
) {
    let now_tokio = tokio::time::Instant::now();
    let now = now_tokio.into_std();
    let decision = desired_state(
        runtime,
        loop_state.state,
        loop_state.peer_observation.as_ref(),
        loop_state.startup_at,
        now,
        loop_state.active_peer_conflict,
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
        dispatch_role_hook(runtime, effect, loop_state.peer_observation.as_ref(), now);
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
    }

    publish_loop_status(runtime, status_tx, loop_state, now);
}

fn dispatch_role_hook(
    runtime: &HaRuntimeConfig,
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
            spawn_hook(runtime.hooks.clone(), HookEvent::Promote, context);
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
            spawn_hook(runtime.hooks.clone(), HookEvent::Demote, context);
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
            spawn_hook(runtime.hooks.clone(), HookEvent::Backup, context);
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
            spawn_hook(runtime.hooks.clone(), HookEvent::Promote, context);
        }
        ReportEffect::Fault {
            state,
            previous_state,
        } => {
            let context = hook_context(
                runtime,
                state,
                previous_state,
                Some(HaDecisionReason::AddressActionFailed),
                loop_state.peer_observation.as_ref(),
                now,
            );
            spawn_hook(runtime.hooks.clone(), HookEvent::Fault, context);
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
    buffer: &[u8; 1_024],
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

    if let Err(error) = handle_incoming_packet(
        runtime,
        payload,
        from,
        observed_at,
        &mut loop_state.peer_observation,
    ) {
        loop_state.counters.invalid_packets = loop_state.counters.invalid_packets.saturating_add(1);
        if is_auth_failure(&error) {
            loop_state.counters.auth_failures = loop_state.counters.auth_failures.saturating_add(1);
        }
        if is_duplicate_node_id_failure(&error) {
            loop_state.counters.duplicate_node_id_packets = loop_state
                .counters
                .duplicate_node_id_packets
                .saturating_add(1);
        }
        warn!(%error, from = %from, "ignoring invalid HA packet");
        return Ok(());
    }

    loop_state.counters.packets_received = loop_state.counters.packets_received.saturating_add(1);
    publish_loop_status(runtime, status_tx, loop_state, observed_at);
    Ok(())
}

async fn handle_send_event(
    socket: &UdpSocket,
    runtime: &HaRuntimeConfig,
    peer_addr: std::net::SocketAddr,
    status_tx: Option<&watch::Sender<HaStatus>>,
    loop_state: &mut HaLoopState,
) -> Result<()> {
    let advertised_state = loop_state.ownership.advertised_role();
    match send_advertisement(
        socket,
        runtime,
        peer_addr,
        advertised_state,
        loop_state.sequence,
    )
    .await
    {
        Ok(sequence) => {
            loop_state.sequence = sequence;
            loop_state.counters.packets_sent = loop_state.counters.packets_sent.saturating_add(1);
        }
        Err(error) if is_transient_send_failure(&error) => {
            warn!(
                %error,
                node_id = %runtime.node_id,
                peer = %peer_addr,
                state = ?advertised_state,
                "HA advertisement send failed; continuing"
            );
        }
        Err(error) => return Err(error),
    }

    loop_state.next_advert_at =
        tokio::time::Instant::now() + runtime.next_advert_delay(loop_state.sequence + 1);
    publish_loop_status(
        runtime,
        status_tx,
        loop_state,
        tokio::time::Instant::now().into_std(),
    );
    Ok(())
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
    faulted: bool,
) {
    let previous_state = loop_state.state;
    let now = tokio::time::Instant::now().into_std();
    let reason = if faulted {
        HaDecisionReason::RuntimeFault
    } else {
        HaDecisionReason::GracefulShutdown
    };

    let withdraw_report = loop_state.ownership.shutdown(worker).await;
    let withdrawal_failed = withdraw_report.kind == ResourceReportKind::Fault;
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
        if let Err(error) = runtime.hooks.run(HookEvent::Fault, context).await {
            warn!(%error, node_id = %runtime.node_id, "HA fault hook execution failed during shutdown");
        }
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
    }

    loop_state.state = HaState::Init;
    loop_state.decision_reason = reason;
    publish_loop_status(runtime, status_tx, loop_state, now);
}

fn handle_incoming_packet(
    runtime: &HaRuntimeConfig,
    payload: &[u8],
    from: std::net::SocketAddr,
    observed_at: Instant,
    peer_observation: &mut Option<PeerObservation>,
) -> Result<()> {
    let packet = HaPacket::decode(payload)?;
    let previous_peer_state = peer_observation.as_ref().and_then(|observation| {
        (observation.node_id == packet.node_id).then_some(observation.state)
    });

    if packet.protocol_version != runtime.protocol_version {
        bail!(
            "unexpected HA protocol version {} from {}",
            packet.protocol_version,
            from
        );
    }

    if packet.group_id != runtime.group_id {
        bail!("unexpected HA group {}", packet.group_id);
    }

    if packet.node_id == runtime.node_id {
        bail!("received HA packet with duplicate node.id");
    }

    if !verify_auth_tag(runtime, &packet) {
        bail!("HA packet authentication failed");
    }

    *peer_observation = Some(PeerObservation {
        node_id: packet.node_id,
        state: packet.state,
        priority: packet.priority,
        became_active: matches!(packet.state, HaState::Active)
            && matches!(
                previous_peer_state,
                Some(previous_state) if previous_state != HaState::Active
            ),
        observed_at,
    });

    debug!(
        node_id = %runtime.node_id,
        peer = %from,
        peer_state = ?packet.state,
        peer_priority = packet.priority,
        "received HA advertisement"
    );

    Ok(())
}

fn build_packet(runtime: &HaRuntimeConfig, state: HaState, sequence: u64) -> Result<HaPacket> {
    let advert_interval_ms = u32::try_from(runtime.advert_interval_ms)
        .map_err(|_| anyhow!("HA advert interval exceeds u32"))?;
    let mut packet = HaPacket {
        protocol_version: runtime.protocol_version,
        state,
        priority: runtime.priority,
        dead_factor: runtime.dead_factor,
        advert_interval_ms,
        sequence,
        node_id: runtime.node_id.clone(),
        group_id: runtime.group_id.clone(),
        auth_tag: Vec::new(),
    };
    packet.auth_tag = auth_tag(&runtime.auth, &packet);

    Ok(packet)
}

async fn send_advertisement(
    socket: &UdpSocket,
    runtime: &HaRuntimeConfig,
    peer_addr: std::net::SocketAddr,
    state: HaState,
    sequence: u64,
) -> Result<u64> {
    let next_sequence = sequence.saturating_add(1);
    let packet = build_packet(runtime, state, next_sequence)?;
    let encoded = packet.encode()?;
    socket
        .send_to(&encoded, peer_addr)
        .await
        .with_context(|| format!("failed to send HA packet to {peer_addr}"))?;

    debug!(
        node_id = %runtime.node_id,
        state = ?state,
        sequence = next_sequence,
        peer = %peer_addr,
        "sent HA advertisement"
    );

    Ok(next_sequence)
}

fn is_auth_failure(error: &anyhow::Error) -> bool {
    error.to_string().contains("authentication failed")
}

fn is_duplicate_node_id_failure(error: &anyhow::Error) -> bool {
    error.to_string().contains("duplicate node.id")
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
        HaRuntimeConfig, HaState, build_packet, handle_incoming_packet,
        is_duplicate_node_id_failure, is_transient_io_error, is_transient_send_failure,
        verify_auth_tag,
    };
    use crate::config::Config;
    use anyhow::Result;
    use std::time::Instant;

    #[test]
    fn rejects_packet_with_invalid_auth_tag() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 127.0.0.1:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: shared_key
    key: shared-secret
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let mut packet = build_packet(&runtime, HaState::Active, 7)?;
        packet.node_id = "peer-a".to_owned();
        packet.auth_tag = vec![0, 1, 2, 3];

        assert!(!verify_auth_tag(&runtime, &packet));

        let mut peer_observation = None;
        let result = handle_incoming_packet(
            &runtime,
            &packet.encode()?,
            "127.0.0.1:9375".parse()?,
            Instant::now(),
            &mut peer_observation,
        );

        assert!(result.is_err());
        assert!(peer_observation.is_none());

        Ok(())
    }

    #[test]
    fn rejects_packet_with_duplicate_node_id() -> Result<()> {
        let config = Config::from_yaml_str(
            r"
mode: ha
node:
  id: node-a
ha:
  interface: eth0
  group_id: cluster-ha
  addresses:
    - 10.0.0.10/24
  peer: 127.0.0.1:9375
  protocol_version: 1
  priority: 100
  advert_interval_ms: 1000
  dead_factor: 3
  hold_down_ms: 3000
  jitter_ms: 100
  auth:
    mode: shared_key
    key: shared-secret
",
        )?;

        let runtime = HaRuntimeConfig::try_from(&config)?;
        let packet = build_packet(&runtime, HaState::Active, 7)?;

        let mut peer_observation = None;
        let result = handle_incoming_packet(
            &runtime,
            &packet.encode()?,
            "127.0.0.1:9375".parse()?,
            Instant::now(),
            &mut peer_observation,
        );

        let Err(error) = result else {
            anyhow::bail!("duplicate node.id packet should be rejected");
        };
        assert!(is_duplicate_node_id_failure(&error));
        assert!(peer_observation.is_none());

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
