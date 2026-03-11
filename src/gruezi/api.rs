use crate::{
    config::DEFAULT_API_PORT,
    gruezi::{ha::HaStatus, net::bind_tcp_listener, status::StatusResponse},
};
use anyhow::Result;
use axum::{Json, Router, extract::State, http::Uri, routing::get};
use std::future::Future;
use tokio::sync::watch;
use tracing::info;

#[derive(Clone)]
struct ApiState {
    status_tx: watch::Sender<HaStatus>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Run the HA management API on the default API port.
///
/// # Errors
///
/// Returns an error if the listener cannot be bound or the HTTP server fails.
pub async fn run_ha_api<F>(status_tx: watch::Sender<HaStatus>, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (listener, bind) = bind_tcp_listener(DEFAULT_API_PORT, None).await?;
    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(ha_status))
        .route("/ha/status", get(ha_status))
        .with_state(ApiState { status_tx });

    info!(bind = %bind, "HA management API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    Ok(())
}

async fn health(uri: Uri) -> Json<HealthResponse> {
    info!(path = %uri.path(), "served HA API health request");
    Json(HealthResponse { status: "ok" })
}

async fn ha_status(State(state): State<ApiState>, uri: Uri) -> Json<StatusResponse> {
    let status = state.status_tx.borrow().clone();
    info!(
        path = %uri.path(),
        node_id = %status.node_id,
        state = ?status.state,
        "served HA API status request"
    );

    Json(StatusResponse::ha(status))
}
