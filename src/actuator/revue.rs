use anyhow::{Context, Result};
use reqwest::Url;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{info, warn};

const MAX_INFLIGHT_TRIGGERS: usize = 16;

#[derive(Clone)]
pub struct RevueClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
    permits: Arc<Semaphore>,
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

impl RevueClient {
    pub fn new(base_url: String, token: Option<String>) -> Result<Self> {
        let parsed = Url::parse(&base_url)
            .with_context(|| format!("invalid CR_STEWARD_REVUE_URL: {base_url}"))?;
        let host = parsed.host_str().unwrap_or("");
        match parsed.scheme() {
            "https" => {}
            "http" if is_loopback_host(host) => {}
            scheme => anyhow::bail!(
                "refusing non-HTTPS revue URL ({scheme}://{host}); HTTP is allowed only for loopback hosts"
            ),
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_TRIGGERS)),
        })
    }

    /// Dispatch a non-blocking advisory review request to `revue` (`POST /api/trigger`).
    /// Returns `true` if accepted for dispatch, `false` if dropped due to backpressure.
    pub fn trigger_advisory_review(&self, repo: &str, pr_number: u64, reason: &str) -> bool {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            warn!(repo, pr_number, "dropping revue trigger: too many in-flight requests");
            return false;
        };
        let client = self.client.clone();
        let url = format!("{}/api/trigger", self.base_url);
        let token = self.token.clone();
        let repo = repo.to_string();
        let reason = reason.to_string();

        tokio::spawn(async move {
            let _permit = permit;
            let mut req = client.post(&url).json(&serde_json::json!({
                "repo": repo,
                "pr": pr_number,
                "reason": reason,
            }));
            if let Some(tok) = &token {
                req = req.bearer_auth(tok);
            }
            match req.send().await.and_then(|r| r.error_for_status()) {
                Ok(_) => info!(repo, pr_number, reason, "triggered advisory revue review"),
                Err(err) => warn!(error = %err, repo, pr_number, reason, "failed to trigger revue"),
            }
        });
        true
    }
}
