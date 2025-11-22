use anyhow::Result;
use crate::gruezi;

/// Execute the peers action
///
/// # Errors
///
/// Returns an error if peer listing fails
pub fn run(format: &str, verbose: bool) -> Result<()> {
    if verbose {
        println!("Output format: {format}");
    }

    // Call the core gruezi peers logic
    let peers = gruezi::peers::list_peers()?;

    match format {
        "json" => {
            if peers.is_empty() {
                println!(r#"{{"peers":[]}}"#);
            } else {
                println!(r#"{{"peers":["#);
                for (i, peer) in peers.iter().enumerate() {
                    println!(
                        r#"  {{"id":"{}","address":"{}","status":"{}","last_seen":"{}"}}{}"#,
                        peer.id,
                        peer.address,
                        peer.status,
                        peer.last_seen,
                        if i < peers.len() - 1 { "," } else { "" }
                    );
                }
                println!(r"]}}");
            }
        }
        "yaml" => {
            if peers.is_empty() {
                println!("peers: []");
            } else {
                println!("peers:");
                for peer in &peers {
                    println!("  - id: {}", peer.id);
                    println!("    address: {}", peer.address);
                    println!("    status: {}", peer.status);
                    println!("    last_seen: {}", peer.last_seen);
                }
            }
        }
        "table" => {
            if peers.is_empty() {
                println!("No peers found");
                println!();
                println!("ID    ADDRESS    STATUS    LAST_SEEN");
                println!("─────────────────────────────────────");
            } else {
                println!("ID              ADDRESS              STATUS         LAST_SEEN");
                println!("────────────────────────────────────────────────────────────────");
                for peer in &peers {
                    println!(
                        "{:<15} {:<20} {:<14} {}",
                        peer.id, peer.address, peer.status, peer.last_seen
                    );
                }
            }
        }
        _ => {
            if verbose {
                println!("Unknown format '{format}', falling back to table output");
            }
            if peers.is_empty() {
                println!("No peers found");
                println!();
                println!("ID    ADDRESS    STATUS    LAST_SEEN");
                println!("─────────────────────────────────────");
            } else {
                println!("ID              ADDRESS              STATUS         LAST_SEEN");
                println!("────────────────────────────────────────────────────────────────");
                for peer in &peers {
                    println!(
                        "{:<15} {:<20} {:<14} {}",
                        peer.id, peer.address, peer.status, peer.last_seen
                    );
                }
            }
        }
    }

    Ok(())
}
