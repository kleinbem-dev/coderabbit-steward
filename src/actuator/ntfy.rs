use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::warn;

const MAX_INFLIGHT_ALERTS: usize = 16;

#[derive(Clone)]
pub struct NtfyClient {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
    permits: Arc<Semaphore>,
}

impl NtfyClient {
    pub fn new(url: String, token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client,
            url,
            token,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_ALERTS)),
        })
    }

    /// Dispatch an ntfy alert on a bounded background task so callers (such as webhook
    /// handlers) never block on a slow ntfy server.
    pub fn send(&self, title: &str, message: &str, priority: &str) {
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            warn!(title, "dropping ntfy alert: too many in-flight deliveries");
            return;
        };
        let client = self.client.clone();
        let url = self.url.clone();
        let token = self.token.clone();
        let title = title.to_string();
        let message = message.to_string();
        let priority = priority.to_string();

        tokio::spawn(async move {
            let _permit = permit;
            let mut req = client
                .post(&url)
                .header("Title", &title)
                .header("Priority", &priority)
                .body(message);
            if let Some(tok) = &token {
                req = req.bearer_auth(tok);
            }
            if let Err(err) = req.send().await.and_then(|r| r.error_for_status()) {
                warn!(error = %err, title, "failed to send ntfy alert");
            }
        });
    }
}
