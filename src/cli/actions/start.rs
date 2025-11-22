use crate::gruezi;
use anyhow::Result;

/// Execute the start action
///
/// # Errors
///
/// Returns an error if the service fails to start
pub fn run(bind: &str, peers: Option<&str>, node_id: Option<&str>, verbose: bool) -> Result<()> {
    if verbose {
        println!("Bind address: {bind}");
        if let Some(p) = peers {
            println!("Peers: {p}");
        }
        if let Some(id) = node_id {
            println!("Node ID: {id}");
        }
    }

    // Call the core gruezi service startup logic
    gruezi::start::start_service(bind, peers, node_id)?;

    println!("Gruezi service started on {bind}");

    Ok(())
}
