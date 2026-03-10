use anyhow::{Context, Result};

/// Wait for a process shutdown signal.
///
/// On Unix this listens for both `SIGINT` and `SIGTERM`.
/// On non-Unix platforms it falls back to `Ctrl-C`.
///
/// # Errors
///
/// Returns an error if installing or waiting on the signal handler fails.
pub async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate =
            signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;

        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for SIGINT")?;
            }
            _ = terminate.recv() => {}
        }

        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for shutdown signal")
    }
}
