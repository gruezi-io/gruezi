use anyhow::Result;
use crate::gruezi;
use crate::gruezi::status::ClusterStatus;

/// Execute the status action
///
/// # Errors
///
/// Returns an error if status retrieval fails
pub fn run(node: Option<&str>, verbose: bool) -> Result<()> {
    if verbose {
        if let Some(n) = node {
            println!("Querying node: {n}");
        } else {
            println!("Querying all nodes");
        }
    }

    // Call the core gruezi status logic
    let status = gruezi::status::get_cluster_status(node)?;
    let ClusterStatus {
        state,
        leader,
        term,
        peer_count,
    } = status;

    println!("Cluster Status:");
    println!("  State: {state}");
    println!("  Leader: {leader}");
    println!("  Term: {term}");
    println!("  Peers: {peer_count}");

    Ok(())
}
