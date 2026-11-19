use crate::gruezi;
use crate::gruezi::ha::PeerRejections;
use anyhow::Result;
use chrono::Local;
use tokio::time::{Duration, sleep};

/// Render the peer session rejection counters as one `key=value` run so both
/// the table and the compact watch line can carry them unchanged.
fn rejection_pairs(rejections: &PeerRejections) -> String {
    format!(
        "wrong_source={} malformed={} unsupported={} bad_tag={} wrong_group={} duplicate_node_id={} wrong_sender={} wrong_receiver={} retired_nonce={} nonce_limit={} duplicate={} too_old={}",
        rejections.wrong_source,
        rejections.malformed,
        rejections.unsupported,
        rejections.bad_tag,
        rejections.wrong_group,
        rejections.duplicate_node_id,
        rejections.wrong_sender,
        rejections.wrong_receiver,
        rejections.retired_nonce,
        rejections.nonce_limit,
        rejections.duplicate,
        rejections.too_old
    )
}

/// Execute the status action.
///
/// # Errors
///
/// Returns an error if status retrieval fails.
pub async fn run(node: Option<&str>, verbose: bool, watch: bool, interval_ms: u64) -> Result<()> {
    if watch {
        return watch_status(node, verbose, interval_ms).await;
    }

    if verbose {
        if let Some(node) = node {
            println!("Querying node: {node}");
        } else {
            println!("Querying local management API");
        }
    }

    let status = gruezi::status::fetch_status(node).await?;
    print_snapshot(&status);

    Ok(())
}

async fn watch_status(node: Option<&str>, verbose: bool, interval_ms: u64) -> Result<()> {
    if verbose {
        if let Some(node) = node {
            println!("Watching node: {node}");
        } else {
            println!("Watching local management API");
        }
        println!("Polling interval: {interval_ms}ms");
        println!("Press Ctrl-C to stop");
    }

    loop {
        let now = Local::now().to_rfc3339();
        match gruezi::status::fetch_status(node).await {
            Ok(status) => println!("{}", format_watch_line(&now, &status)),
            Err(error) => eprintln!("[{now}] status query failed: {error}"),
        }

        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
            () = sleep(Duration::from_millis(interval_ms)) => {}
        }
    }

    Ok(())
}

fn print_snapshot(status: &gruezi::status::StatusResponse) {
    for line in snapshot_lines(status) {
        println!("{line}");
    }
}

fn snapshot_lines(status: &gruezi::status::StatusResponse) -> Vec<String> {
    let mut lines = vec![
        "Node Status:".to_owned(),
        format!("  Mode: {}", status.mode),
    ];

    if let Some(ha) = &status.ha {
        lines.push(format!("  Node ID: {}", ha.node_id));
        lines.push(format!("  Group ID: {}", ha.group_id));
        lines.push(format!("  Elected Role: {:?}", ha.state));
        lines.push(format!("  Owned: {}", ha.owned));
        lines.push(format!(
            "  Resource: {} confirmed={} observed_ago_ms={:?} last_error={:?}",
            ha.resource.state,
            ha.resource.confirmed,
            ha.resource.observed_ago_ms,
            ha.resource.last_error
        ));
        lines.push(format!(
            "  Authority: {} source={} term={} expires_in_ms={:?} grant_expires_in_ms={:?} fencing={}",
            ha.authority.state,
            ha.authority.source.map_or("none", |source| source.as_str()),
            ha.authority.term,
            ha.authority.expires_in_ms,
            ha.authority.grant_expires_in_ms,
            ha.authority.fencing.as_str()
        ));
        lines.push(format!("  Failure Policy: {}", ha.failure_policy));
        lines.push(format!(
            "  Watchdog: mode={} armed={} effective_timeout_ms={:?} last_feed_ago_ms={:?} unprotected={}",
            ha.watchdog.mode,
            ha.watchdog.armed,
            ha.watchdog.effective_timeout_ms,
            ha.watchdog.last_feed_ago_ms,
            ha.watchdog.unprotected
        ));
        lines.push(format!(
            "  Safety: {} ({})",
            ha.safety.level.as_str(),
            ha.safety.why
        ));
        lines.push(format!("  Bind: {}", ha.bind));
        lines.push(format!("  Peer: {}", ha.peer));
        lines.push(format!("  Priority: {}", ha.priority));
        lines.push(format!("  Advert Interval: {}ms", ha.advert_interval_ms));
        lines.push(format!("  Dead Timeout: {}ms", ha.dead_timeout_ms));
        lines.push(format!("  Hold Down: {}ms", ha.hold_down_ms));
        lines.push(format!("  Peer Alive: {}", ha.peer_alive));
        lines.push(format!(
            "  Last Peer Seen: {}",
            ha.last_peer_seen_ms_ago
                .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"))
        ));
        lines.push(format!(
            "  Peer Node ID: {}",
            ha.peer_node_id.as_deref().unwrap_or("unknown")
        ));
        lines.push(format!(
            "  Peer State: {}",
            ha.peer_state
                .map_or_else(|| "unknown".to_owned(), |state| format!("{state:?}"))
        ));
        lines.push(format!("  Sequence: {}", ha.sequence));
        lines.push(format!("  Packets Sent: {}", ha.packets_sent));
        lines.push(format!("  Packets Received: {}", ha.packets_received));
        lines.push(format!("  Invalid Packets: {}", ha.invalid_packets));
        lines.push(format!("  Auth Failures: {}", ha.auth_failures));
        lines.push(format!(
            "  Duplicate Node ID Packets: {}",
            ha.duplicate_node_id_packets
        ));
        lines.push(format!(
            "  Peer Session: reachable={} session_established={}",
            ha.peer_session.reachable, ha.peer_session.session_established
        ));
        lines.push(format!(
            "  Session Rejections: {}",
            rejection_pairs(&ha.peer_session.rejections)
        ));
        lines.push(format!("  Decision Reason: {}", ha.decision_reason));
        lines.push(format!(
            "  Last Transition Reason: {}",
            ha.last_transition_reason
                .map_or_else(|| "unknown".to_owned(), |reason| reason.to_string())
        ));
        lines.push(format!(
            "  Last Transition: {}",
            ha.last_transition_ms_ago
                .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"))
        ));
        if let Some(transition) = ha.last_transition {
            lines.push(format!(
                "  Last Transition Detail: reason={} trigger={} ago_ms={}",
                transition.reason,
                transition.trigger.as_str(),
                transition.ago_ms
            ));
        }
    }

    lines
}

fn format_watch_line(timestamp: &str, status: &gruezi::status::StatusResponse) -> String {
    if let Some(ha) = &status.ha {
        let peer_state = ha
            .peer_state
            .map_or_else(|| "unknown".to_owned(), |state| format!("{state:?}"));
        let peer_node_id = ha.peer_node_id.as_deref().unwrap_or("unknown");
        let last_peer_seen = ha
            .last_peer_seen_ms_ago
            .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"));
        let session_rejections = rejection_pairs(&ha.peer_session.rejections);

        format!(
            "[{timestamp}] node={} elected_role={:?} owned={} resource={} confirmed={} observed_ago_ms={:?} last_error={:?} authority={} authority_source={} term={} authority_expires_in_ms={:?} grant_expires_in_ms={:?} fencing={} failure_policy={} watchdog_mode={} watchdog_armed={} watchdog_effective_timeout_ms={:?} watchdog_last_feed_ago_ms={:?} watchdog_unprotected={} safety={} safety_why={:?} reason={} last_transition_reason={} last_transition_trigger={} last_transition_ago_ms={} peer_alive={} peer_node={} peer_state={} peer_reachable={} session_established={} session_rejections={:?} seq={} sent={} recv={} invalid={} auth_failures={} duplicate_node_id_packets={} last_peer_seen={} last_transition={}",
            ha.node_id,
            ha.state,
            ha.owned,
            ha.resource.state,
            ha.resource.confirmed,
            ha.resource.observed_ago_ms,
            ha.resource.last_error,
            ha.authority.state,
            ha.authority.source.map_or("none", |source| source.as_str()),
            ha.authority.term,
            ha.authority.expires_in_ms,
            ha.authority.grant_expires_in_ms,
            ha.authority.fencing.as_str(),
            ha.failure_policy,
            ha.watchdog.mode,
            ha.watchdog.armed,
            ha.watchdog.effective_timeout_ms,
            ha.watchdog.last_feed_ago_ms,
            ha.watchdog.unprotected,
            ha.safety.level.as_str(),
            ha.safety.why,
            ha.decision_reason,
            ha.last_transition_reason
                .map_or_else(|| "unknown".to_owned(), |reason| reason.to_string()),
            ha.last_transition
                .map_or("unknown", |transition| transition.trigger.as_str()),
            ha.last_transition.map_or_else(
                || "unknown".to_owned(),
                |transition| transition.ago_ms.to_string()
            ),
            ha.peer_alive,
            peer_node_id,
            peer_state,
            ha.peer_session.reachable,
            ha.peer_session.session_established,
            session_rejections,
            ha.sequence,
            ha.packets_sent,
            ha.packets_received,
            ha.invalid_packets,
            ha.auth_failures,
            ha.duplicate_node_id_packets,
            last_peer_seen,
            ha.last_transition_ms_ago
                .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"))
        )
    } else {
        format!("[{timestamp}] mode={}", status.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_watch_line, snapshot_lines};
    use crate::gruezi::ha::{
        HaDecisionReason, HaState, HaStatus, PeerRejections, PeerSessionStatus,
    };
    use crate::gruezi::status::StatusResponse;
    use crate::gruezi::watchdog::WatchdogStatus;

    fn sample_status() -> StatusResponse {
        let mut status = HaStatus {
            node_id: "gruezi-a".to_owned(),
            group_id: "lab-ha".to_owned(),
            bind: "0.0.0.0:9375".to_owned(),
            peer: "192.0.2.11:9375".to_owned(),
            state: HaState::Active,
            priority: 150,
            advert_interval_ms: 1_000,
            dead_timeout_ms: 3_000,
            hold_down_ms: 3_000,
            sequence: 42,
            peer_node_id: Some("gruezi-b".to_owned()),
            peer_state: Some(HaState::Standby),
            peer_alive: true,
            last_peer_seen_ms_ago: Some(17),
            packets_sent: 42,
            packets_received: 41,
            invalid_packets: 1,
            auth_failures: 0,
            duplicate_node_id_packets: 0,
            decision_reason: HaDecisionReason::PeerTimeout,
            last_transition_reason: Some(HaDecisionReason::LocalHigherPriority),
            last_transition_ms_ago: Some(250),
            peer_session: PeerSessionStatus {
                reachable: true,
                session_established: true,
                rejections: PeerRejections {
                    bad_tag: 2,
                    duplicate: 1,
                    ..PeerRejections::default()
                },
            },
            ..HaStatus::new("gruezi-a", "lab-ha", "0.0.0.0:9375", "192.0.2.11:9375")
        };
        status.failure_policy = "strict".to_owned();
        status.watchdog = WatchdogStatus {
            mode: "automatic".to_owned(),
            armed: true,
            effective_timeout_ms: Some(5_000),
            last_feed_ago_ms: Some(250),
            unprotected: false,
        };
        status.authority.expires_in_ms = Some(4_000);
        status.authority.grant_expires_in_ms = Some(3_000);
        status.authority.fencing = crate::gruezi::ownership::FencingState::Confirmed;
        StatusResponse::ha(status)
    }

    #[test]
    fn snapshot_includes_ha_counters_and_timers() {
        let lines = snapshot_lines(&sample_status());
        let output = lines.join("\n");
        println!("{output}");
        assert!(output.contains("Group ID: lab-ha"));
        assert!(output.contains("Dead Timeout: 3000ms"));
        assert!(output.contains("Last Peer Seen: 17ms"));
        assert!(output.contains("Packets Received: 41"));
        assert!(output.contains("Duplicate Node ID Packets: 0"));
        assert!(output.contains("Decision Reason: peer_timeout"));
        assert!(output.contains("Last Transition Reason: local_higher_priority"));
        assert!(output.contains("Elected Role: Active"));
        assert!(output.contains("Owned: false"));
        assert!(output.contains("Resource: releasing confirmed=false"));
        assert!(output.contains("Authority: none source=none term=0"));
        assert!(output.contains("expires_in_ms=Some(4000)"));
        assert!(output.contains("grant_expires_in_ms=Some(3000)"));
        assert!(output.contains("fencing=confirmed"));
        assert!(output.contains("Failure Policy: strict"));
        assert!(output.contains("Watchdog: mode=automatic armed=true"));
        assert!(output.contains("effective_timeout_ms=Some(5000)"));
        assert!(output.contains("Safety: uncertain"));
        assert!(output.contains("Peer Session: reachable=true session_established=true"));
        assert!(output.contains("Session Rejections: wrong_source=0"));
        assert!(output.contains("bad_tag=2"));
        assert!(output.contains("too_old=0"));
    }

    #[test]
    fn watch_line_is_timestamped_and_compact() {
        let line = format_watch_line("2026-03-10T12:00:00+01:00", &sample_status());
        println!("{line}");
        assert!(line.contains("[2026-03-10T12:00:00+01:00]"));
        assert!(line.contains("node=gruezi-a"));
        assert!(line.contains("elected_role=Active"));
        assert!(line.contains("owned=false"));
        assert!(line.contains("resource=releasing"));
        assert!(line.contains("safety=uncertain"));
        assert!(line.contains("reason=peer_timeout"));
        assert!(line.contains("recv=41"));
        assert!(line.contains("duplicate_node_id_packets=0"));
        assert!(line.contains("peer_reachable=true"));
        assert!(line.contains("session_established=true"));
        assert!(line.contains("session_rejections=\"wrong_source=0"));
        assert!(line.contains("bad_tag=2"));
        assert!(line.contains("last_peer_seen=17ms"));
        assert!(line.contains("last_transition=250ms"));
        assert!(line.contains("authority_expires_in_ms=Some(4000)"));
        assert!(line.contains("grant_expires_in_ms=Some(3000)"));
        assert!(line.contains("fencing=confirmed"));
        assert!(line.contains("failure_policy=strict"));
        assert!(line.contains("watchdog_mode=automatic"));
        assert!(line.contains("watchdog_armed=true"));
        assert!(line.contains("watchdog_unprotected=false"));
        assert!(line.contains("last_transition_trigger=unknown"));
    }

    #[test]
    fn snapshot_and_watch_render_unarmed_watchdog() {
        let mut status = sample_status();
        if let Some(ha) = status.ha.as_mut() {
            ha.watchdog = WatchdogStatus {
                mode: "required".to_owned(),
                armed: false,
                effective_timeout_ms: None,
                last_feed_ago_ms: None,
                unprotected: true,
            };
        }
        let snapshot = snapshot_lines(&status).join("\n");
        let watch = format_watch_line("2026-03-10T12:00:00+01:00", &status);
        println!("{snapshot}\n{watch}");
        assert!(snapshot.contains(
            "Watchdog: mode=required armed=false effective_timeout_ms=None last_feed_ago_ms=None unprotected=true"
        ));
        assert!(watch.contains("watchdog_mode=required"));
        assert!(watch.contains("watchdog_armed=false"));
        assert!(watch.contains("watchdog_unprotected=true"));
    }
}
