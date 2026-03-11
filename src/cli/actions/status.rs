use crate::gruezi;
use anyhow::Result;
use chrono::Local;
use tokio::time::{Duration, sleep};

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
        lines.push(format!("  HA State: {:?}", ha.state));
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

        format!(
            "[{timestamp}] node={} state={:?} reason={} last_transition_reason={} peer_alive={} peer_node={} peer_state={} seq={} sent={} recv={} invalid={} auth_failures={} duplicate_node_id_packets={} last_peer_seen={} last_transition={}",
            ha.node_id,
            ha.state,
            ha.decision_reason,
            ha.last_transition_reason
                .map_or_else(|| "unknown".to_owned(), |reason| reason.to_string()),
            ha.peer_alive,
            peer_node_id,
            peer_state,
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
    use crate::gruezi::ha::{HaDecisionReason, HaState, HaStatus};
    use crate::gruezi::status::StatusResponse;

    fn sample_status() -> StatusResponse {
        StatusResponse::ha(HaStatus {
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
        })
    }

    #[test]
    fn snapshot_includes_ha_counters_and_timers() {
        let lines = snapshot_lines(&sample_status());
        let output = lines.join("\n");
        assert!(output.contains("Group ID: lab-ha"));
        assert!(output.contains("Dead Timeout: 3000ms"));
        assert!(output.contains("Last Peer Seen: 17ms"));
        assert!(output.contains("Packets Received: 41"));
        assert!(output.contains("Duplicate Node ID Packets: 0"));
        assert!(output.contains("Decision Reason: peer_timeout"));
        assert!(output.contains("Last Transition Reason: local_higher_priority"));
    }

    #[test]
    fn watch_line_is_timestamped_and_compact() {
        let line = format_watch_line("2026-03-10T12:00:00+01:00", &sample_status());
        assert!(line.contains("[2026-03-10T12:00:00+01:00]"));
        assert!(line.contains("node=gruezi-a"));
        assert!(line.contains("state=Active"));
        assert!(line.contains("reason=peer_timeout"));
        assert!(line.contains("recv=41"));
        assert!(line.contains("duplicate_node_id_packets=0"));
        assert!(line.contains("last_peer_seen=17ms"));
        assert!(line.contains("last_transition=250ms"));
    }
}
