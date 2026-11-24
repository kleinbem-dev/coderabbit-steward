use crate::config::Config;
use anyhow::{Context, Result};
use jiff::Timestamp;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Debug, Deserialize)]
struct RepoInstallation {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResp {
    token: String,
}

struct CachedToken {
    token: String,
    fetched_at: Instant,
}

pub struct GitHubActuator {
    client: reqwest::Client,
    config: Config,
    installation_tokens: HashMap<String, CachedToken>,
}

impl GitHubActuator {
    pub fn new(config: Config) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            config,
            installation_tokens: HashMap::new(),
        })
    }

    /// Re-trigger a CodeRabbit review on a PR by removing and re-adding `cr-review`.
    /// Requires a human user PAT (`CR_STEWARD_USER_TOKEN`) because CodeRabbit intentionally
    /// ignores label events authored by GitHub Apps to avoid bot loops.
    pub async fn retrigger_cr_review_label(&self, repo: &str, pr_number: u64) -> Result<()> {
        let token = self
            .config
            .github_user_token
            .as_deref()
            .context("CR_STEWARD_USER_TOKEN not configured; cannot re-trigger cr-review label")?;

        let del_url = format!(
            "{}/repos/{repo}/issues/{pr_number}/labels/cr-review",
            self.config.github_api_base
        );
        let del_resp = self
            .client
            .delete(&del_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;
        if del_resp.status() != reqwest::StatusCode::NOT_FOUND {
            del_resp.error_for_status()?;
        }

        let add_url = format!(
            "{}/repos/{repo}/issues/{pr_number}/labels",
            self.config.github_api_base
        );
        self.client
            .post(&add_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "labels": ["cr-review"] }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Create an advisory `cr-steward` check run on a PR head commit via the GitHub App.
    /// Uses an App installation token exclusively (the check-runs endpoint rejects PATs).
    pub async fn post_steward_check_run(
        &mut self,
        repo: &str,
        head_sha: &str,
        title: &str,
        summary: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> Result<()> {
        let token = self.installation_token_for_repo(repo).await?;
        let url = format!("{}/repos/{repo}/check-runs", self.config.github_api_base);
        let mut body = serde_json::json!({
            "name": "cr-steward",
            "head_sha": head_sha,
            "status": status,
            "output": {
                "title": title,
                "summary": summary,
            }
        });
        if let Some(c) = conclusion {
            body["conclusion"] = serde_json::Value::String(c.to_string());
        }

        self.client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn get_pr_head_sha(&mut self, repo: &str, pr_number: u64) -> Result<Option<String>> {
        let token = self.read_token_for_repo(repo).await?;
        let url = format!("{}/repos/{repo}/pulls/{pr_number}", self.config.github_api_base);
        let resp: serde_json::Value = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp
            .get("head")
            .and_then(|h| h.get("sha"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string))
    }

    fn load_private_key_pem(&self) -> Result<String> {
        if let Some(pem) = &self.config.github_private_key_pem {
            return Ok(pem.clone());
        }
        if let Some(path) = &self.config.github_private_key_path {
            return std::fs::read_to_string(path)
                .with_context(|| format!("failed to read app private key from {}", path.display()));
        }
        anyhow::bail!("no GitHub App private key configured")
    }

    fn make_app_jwt(&self) -> Result<String> {
        let app_id = self
            .config
            .github_app_id
            .context("github_app_id not configured")?;
        let pem = self.load_private_key_pem()?;
        let now = Timestamp::now().as_second();
        let claims = AppClaims {
            iat: now - 60,
            exp: now + 540,
            iss: app_id.to_string(),
        };
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .context("invalid RSA private key PEM for GitHub App")?;
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
            .context("failed to sign GitHub App JWT")
    }

    async fn read_token_for_repo(&mut self, repo: &str) -> Result<String> {
        if let Some(tok) = &self.config.github_token {
            return Ok(tok.clone());
        }
        self.installation_token_for_repo(repo).await
    }

    async fn installation_token_for_repo(&mut self, repo: &str) -> Result<String> {
        let owner = repo.split('/').next().unwrap_or(repo);
        if let Some(cached) = self.installation_tokens.get(owner)
            && cached.fetched_at.elapsed() < Duration::from_secs(45 * 60)
        {
            return Ok(cached.token.clone());
        }

        let jwt = self.make_app_jwt()?;
        let url = format!("{}/repos/{repo}/installation", self.config.github_api_base);
        let inst: RepoInstallation = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let tok_url = format!(
            "{}/app/installations/{}/access_tokens",
            self.config.github_api_base, inst.id
        );
        let resp: InstallationTokenResp = self
            .client
            .post(&tok_url)
            .header(AUTHORIZATION, format!("Bearer {jwt}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        self.installation_tokens.insert(
            owner.to_string(),
            CachedToken {
                token: resp.token.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(resp.token)
    }
}
