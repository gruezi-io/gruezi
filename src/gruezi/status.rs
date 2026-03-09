use crate::config::DEFAULT_API_PORT;
use crate::gruezi::ha::HaStatus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub mode: String,
    pub ha: Option<HaStatus>,
}

impl StatusResponse {
    #[must_use]
    pub fn ha(status: HaStatus) -> Self {
        Self {
            mode: "ha".to_owned(),
            ha: Some(status),
        }
    }
}

/// Fetch node status from the management API.
///
/// # Errors
///
/// Returns an error if the request or response parsing fails.
pub async fn fetch_status(node: Option<&str>) -> Result<StatusResponse> {
    let endpoint = status_endpoint(node);
    let response = reqwest::Client::new()
        .get(&endpoint)
        .send()
        .await
        .with_context(|| format!("failed to query status endpoint {endpoint}"))?
        .error_for_status()
        .with_context(|| format!("status endpoint returned an error for {endpoint}"))?;

    response
        .json::<StatusResponse>()
        .await
        .with_context(|| format!("failed to decode status response from {endpoint}"))
}

fn status_endpoint(node: Option<&str>) -> String {
    node.map_or_else(
        || format!("http://127.0.0.1:{DEFAULT_API_PORT}/status"),
        normalize_status_endpoint,
    )
}

fn normalize_status_endpoint(node: &str) -> String {
    if node.starts_with("http://") || node.starts_with("https://") {
        if node.ends_with("/status") {
            node.to_owned()
        } else {
            format!("{}/status", node.trim_end_matches('/'))
        }
    } else {
        format!("http://{node}/status")
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_status_endpoint;

    #[test]
    fn normalizes_bare_host_and_port() {
        assert_eq!(
            normalize_status_endpoint("127.0.0.1:9376"),
            "http://127.0.0.1:9376/status"
        );
    }

    #[test]
    fn preserves_existing_status_path() {
        assert_eq!(
            normalize_status_endpoint("http://127.0.0.1:9376/status"),
            "http://127.0.0.1:9376/status"
        );
    }
}
