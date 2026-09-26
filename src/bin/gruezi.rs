use anyhow::Result;
use gruezi::cli::{actions::Action, start, telemetry};

#[tokio::main]
async fn main() -> Result<()> {
    let result = match start() {
        Ok(action) => execute(action).await,
        Err(error) => Err(error),
    };
    let _ = telemetry::shutdown_tracer();
    result
}

/// Execute the given action
#[tracing::instrument(name = "gruezi.execute", skip(action))]
async fn execute(action: Action) -> Result<()> {
    match action {
        Action::Start {
            config,
            bind,
            peers,
            node_id,
            verbose,
        } => {
            gruezi::cli::actions::start::run(
                config.as_deref(),
                &bind,
                peers.as_deref(),
                node_id.as_deref(),
                verbose,
            )
            .await?;
        }
        Action::Status {
            node,
            verbose,
            watch,
            interval_ms,
        } => {
            gruezi::cli::actions::status::run(node.as_deref(), verbose, watch, interval_ms).await?;
        }
        Action::Peers { format, verbose } => {
            gruezi::cli::actions::peers::run(&format, verbose)?;
        }
    }

    Ok(())
}
