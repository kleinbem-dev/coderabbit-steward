use anyhow::Result;
use std::time::Duration;
use tracing::warn;

#[derive(Clone)]
pub struct NtfyClient {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl NtfyClient {
    pub fn new(url: String, token: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { client, url, token })
    }

    pub async fn send(&self, title: &str, message: &str, priority: &str) {
        let mut req = self
            .client
            .post(&self.url)
            .header("Title", title)
            .header("Priority", priority)
            .body(message.to_string());
        if let Some(tok) = &self.token {
            req = req.bearer_auth(tok);
        }
        if let Err(err) = req.send().await.and_then(|r| r.error_for_status()) {
            warn!(error = %err, title, "failed to send ntfy alert");
        }
    }
}
