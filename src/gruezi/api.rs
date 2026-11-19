use crate::gruezi::{ha::HaStatus, status::StatusResponse};
use anyhow::Result;
use axum::{Json, Router, extract::State, http::Uri, routing::get};
use std::future::Future;
use tokio::{net::TcpListener, sync::watch};
use tracing::info;

#[derive(Clone)]
struct ApiState {
    status_tx: watch::Sender<HaStatus>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Run the HA management API on a pre-bound listener.
///
/// # Errors
///
/// Returns an error if the listener address cannot be read or the HTTP server fails.
pub async fn run_ha_api<F>(
    listener: TcpListener,
    status_tx: watch::Sender<HaStatus>,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let bind = listener.local_addr()?;
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

#[cfg(test)]
mod tests {
    use super::run_ha_api;
    use crate::gruezi::ha::{HaDecisionReason, HaState, HaStatus};
    use anyhow::Result;
    use tokio::{
        net::TcpListener,
        sync::{oneshot, watch},
        time::{Duration, timeout},
    };

    #[tokio::test]
    async fn status_json_preserves_every_existing_field_and_health() -> Result<()> {
        let status = HaStatus {
            state: HaState::Active,
            priority: 110,
            advert_interval_ms: 40,
            dead_timeout_ms: 80,
            hold_down_ms: 40,
            sequence: 7,
            peer_node_id: Some("node-b".to_owned()),
            peer_state: Some(HaState::Standby),
            peer_alive: true,
            last_peer_seen_ms_ago: Some(3),
            packets_sent: 7,
            packets_received: 6,
            invalid_packets: 5,
            auth_failures: 4,
            duplicate_node_id_packets: 2,
            decision_reason: HaDecisionReason::AlreadyActive,
            last_transition_reason: Some(HaDecisionReason::LocalHigherPriority),
            last_transition_ms_ago: Some(10),
            ..HaStatus::new("node-a", "cluster-ha", "127.0.0.1:1", "127.0.0.1:2")
        };
        let (status_tx, _) = watch::channel(status);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_ha_api(listener, status_tx, async {
            let _ = shutdown_rx.await;
        }));
        let client = reqwest::Client::new();
        for path in ["/status", "/ha/status"] {
            let response = timeout(
                Duration::from_secs(3),
                client.get(format!("http://{addr}{path}")).send(),
            )
            .await??
            .error_for_status()?
            .text()
            .await?;
            // Compare the entire wire response so a renamed, omitted, or
            // changed legacy field cannot hide behind a successful decode.
            // Additive schema fields also update this golden payload.
            assert_eq!(
                response,
                concat!(
                    r#"{"mode":"ha","ha":{"node_id":"node-a","group_id":"cluster-ha","#,
                    r#""bind":"127.0.0.1:1","peer":"127.0.0.1:2","state":"Active","owned":false,"#,
                    r#""resource":{"state":"releasing","confirmed":false,"observed_ago_ms":null,"last_error":null},"#,
                    r#""authority":{"state":"none","source":null,"term":0,"expires_in_ms":null,"#,
                    r#""grant_expires_in_ms":null,"fencing":"none"},"#,
                    r#""safety":{"level":"uncertain","why":"resource or authority is not confirmed"},"#,
                    r#""priority":110,"advert_interval_ms":40,"dead_timeout_ms":80,"hold_down_ms":40,"#,
                    r#""sequence":7,"peer_node_id":"node-b","peer_state":"Standby","peer_alive":true,"#,
                    r#""last_peer_seen_ms_ago":3,"packets_sent":7,"packets_received":6,"invalid_packets":5,"#,
                    r#""auth_failures":4,"duplicate_node_id_packets":2,"decision_reason":"already_active","#,
                    r#""last_transition_reason":"local_higher_priority","last_transition_ms_ago":10,"#,
                    r#""last_transition":null,"failure_policy":"","#,
                    r#""peer_session":{"reachable":false,"session_established":false,"#,
                    r#""rejections":{"wrong_source":0,"malformed":0,"unsupported":0,"bad_tag":0,"#,
                    r#""wrong_group":0,"duplicate_node_id":0,"wrong_sender":0,"wrong_receiver":0,"#,
                    r#""retired_nonce":0,"nonce_limit":0,"duplicate":0,"too_old":0}},"#,
                    r#""watchdog":{"mode":"off","armed":false,"effective_timeout_ms":null,"#,
                    r#""last_feed_ago_ms":null,"unprotected":false},"#,
                    r#""lifecycle":{"configured":false,"phase":"not_configured","generation":0,"operation":null,"#,
                    r#""activation_confirmed":false,"passive_confirmed":false,"verified_fresh":false,"#,
                    r#""verification":{"state":"not_configured","last_verified_ago_ms":null,"next_verify_in_ms":null,"db_incarnation":null},"#,
                    r#""last_critical_failure":null},"instance_id":""}}"#,
                )
            );
        }
        let health = client
            .get(format!("http://{addr}/health"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        assert_eq!(health, r#"{"status":"ok"}"#);
        let _ = shutdown_tx.send(());
        timeout(Duration::from_secs(3), task).await???;
        Ok(())
    }
}
