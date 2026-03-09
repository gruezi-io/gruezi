use crate::gruezi;
use anyhow::Result;

/// Execute the status action.
///
/// # Errors
///
/// Returns an error if status retrieval fails.
pub async fn run(node: Option<&str>, verbose: bool) -> Result<()> {
    if verbose {
        if let Some(node) = node {
            println!("Querying node: {node}");
        } else {
            println!("Querying local management API");
        }
    }

    let status = gruezi::status::fetch_status(node).await?;

    println!("Node Status:");
    println!("  Mode: {}", status.mode);

    if let Some(ha) = status.ha {
        println!("  Node ID: {}", ha.node_id);
        println!("  HA State: {:?}", ha.state);
        println!("  Peer: {}", ha.peer);
        println!("  Peer Alive: {}", ha.peer_alive);
        println!(
            "  Peer Node ID: {}",
            ha.peer_node_id.as_deref().unwrap_or("unknown")
        );
        println!(
            "  Peer State: {}",
            ha.peer_state
                .map_or_else(|| "unknown".to_owned(), |state| format!("{state:?}"))
        );
        println!("  Sequence: {}", ha.sequence);
        println!("  Packets Sent: {}", ha.packets_sent);
        println!("  Packets Received: {}", ha.packets_received);
        println!("  Invalid Packets: {}", ha.invalid_packets);
        println!("  Auth Failures: {}", ha.auth_failures);
    }

    Ok(())
}
