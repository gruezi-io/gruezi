use anyhow::Result;
use gruezi::cli::{actions::Action, start};

fn main() -> Result<()> {
    // Initialize CLI and get the action to execute
    let action = start()?;

    // Execute the action
    execute(action)?;

    Ok(())
}

/// Execute the given action
fn execute(action: Action) -> Result<()> {
    match action {
        Action::Start {
            bind,
            peers,
            node_id,
            verbose,
        } => {
            gruezi::cli::actions::start::run(
                &bind,
                peers.as_deref(),
                node_id.as_deref(),
                verbose,
            )
            ?;
        }
        Action::Status { node, verbose } => {
            gruezi::cli::actions::status::run(node.as_deref(), verbose)?;
        }
        Action::Peers { format, verbose } => {
            gruezi::cli::actions::peers::run(&format, verbose)?;
        }
    }

    Ok(())
}
