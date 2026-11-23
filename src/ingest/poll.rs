use crate::config::Config;
use crate::db::Db;
use crate::parse::check_run::parse_check_run;
use crate::parse::comment::{BotMessage, parse_issue_comment};
use crate::parse::review::parse_pr_review;
use anyhow::{Context, Result};
use jiff::Timestamp;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::header::{AUTHORIZATION, ETAG, IF_NONE_MATCH, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

#[derive(Debug, Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
    account: InstallationAccount,
}

#[derive(Debug, Deserialize)]
struct InstallationAccount {
    login: String,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResp {
    token: String,
}

#[derive(Debug, Deserialize)]
struct GhRepo {
    name: String,
    owner: GhOwner,
    #[serde(rename = "private")]
    is_private: bool,
    stargazers_count: i64,
    default_branch: Option<String>,
    archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GhOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPull {
    number: u64,
    user: Option<GhOwner>,
    head: GhHead,
}

#[derive(Debug, Deserialize)]
struct GhHead {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GhCheckRunsResp {
    check_runs: Vec<Value>,
}

struct CachedToken {
    token: String,
    fetched_at: Instant,
}

pub struct GitHubPoller {
    client: reqwest::Client,
    config: Config,
    db: Db,
    installation_tokens: HashMap<String, CachedToken>,
}

impl GitHubPoller {
    pub fn new(config: Config, db: Db) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            config,
            db,
            installation_tokens: HashMap::new(),
        })
    }

    pub fn has_credentials(&self) -> bool {
        self.config.github_token.is_some()
            || (self.config.github_app_id.is_some()
                && (self.config.github_private_key_pem.is_some()
                    || self.config.github_private_key_path.is_some()))
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

    async fn token_for_owner(&mut self, owner: &str) -> Result<String> {
        if let Some(tok) = &self.config.github_token {
            return Ok(tok.clone());
        }
        if let Some(cached) = self.installation_tokens.get(owner)
            && cached.fetched_at.elapsed() < Duration::from_secs(45 * 60)
        {
            return Ok(cached.token.clone());
        }

        let jwt = self.make_app_jwt()?;
        let url = format!("{}/app/installations", self.config.github_api_base);
        let installs: Vec<Installation> = self
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

        let inst = installs
            .into_iter()
            .find(|i| i.account.login.eq_ignore_ascii_case(owner))
            .with_context(|| format!("no GitHub App installation found for owner {owner}"))?;

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

    async fn get_with_etag(&self, url: &str, token: &str) -> Result<Option<Value>> {
        let mut req = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json");

        if let Some(etag) = self.db.get_etag(url).await? {
            req = req.header(IF_NONE_MATCH, etag);
        }

        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            debug!(url, "304 Not Modified");
            return Ok(None);
        }
        let new_etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        let val: Value = resp.error_for_status()?.json().await?;
        if let Some(etag) = new_etag {
            self.db.set_etag(url, &etag).await?;
        }
        Ok(Some(val))
    }

    pub async fn poll_once(&mut self) -> Result<()> {
        if !self.has_credentials() {
            debug!("skipping GitHub poll: no credentials configured");
            return Ok(());
        }

        let owners = self.config.owners.clone();
        for owner in owners {
            let token = match self.token_for_owner(&owner).await {
                Ok(t) => t,
                Err(err) => {
                    warn!(owner, error = %err, "failed to obtain GitHub token");
                    continue;
                }
            };

            let repos_url = format!(
                "{}/users/{owner}/repos?per_page=100&type=owner",
                self.config.github_api_base
            );
            if let Some(val) = self.get_with_etag(&repos_url, &token).await?
                && let Ok(repos) = serde_json::from_value::<Vec<GhRepo>>(val)
            {
                for repo in repos {
                    if repo.archived.unwrap_or(false) || repo.is_private {
                        continue;
                    }
                    let branch = repo.default_branch.as_deref().unwrap_or("main");
                    self.db
                        .upsert_repo(
                            &repo.owner.login,
                            &repo.name,
                            "public",
                            repo.stargazers_count,
                            branch,
                        )
                        .await?;
                }
            }

            let tracked = self.db.list_repos().await?;
            for repo in tracked.into_iter().filter(|r| r.owner.eq_ignore_ascii_case(&owner)) {
                let full = format!("{}/{}", repo.owner, repo.name);
                if let Err(err) = self.poll_repo(&full, &token).await {
                    warn!(repo = full, error = %err, "poll_repo failed");
                }
            }
        }
        Ok(())
    }

    async fn poll_repo(&self, repo: &str, token: &str) -> Result<()> {
        let pulls_url = format!(
            "{}/repos/{repo}/pulls?state=all&sort=updated&direction=desc&per_page=10",
            self.config.github_api_base
        );
        let Some(pulls_val) = self.get_with_etag(&pulls_url, token).await? else {
            return Ok(());
        };
        let pulls: Vec<GhPull> = serde_json::from_value(pulls_val)?;
        let mut ledger = self.db.load_budget_ledger(repo).await?;
        let now = Timestamp::now();
        let mut changed = false;

        for pr in pulls {
            let author = pr
                .user
                .as_ref()
                .map(|u| u.login.as_str())
                .unwrap_or("unknown");

            let checks_url = format!(
                "{}/repos/{repo}/commits/{}/check-runs?per_page=50",
                self.config.github_api_base, pr.head.sha
            );
            if let Some(val) = self.get_with_etag(&checks_url, token).await?
                && let Ok(resp) = serde_json::from_value::<GhCheckRunsResp>(val)
            {
                for cr in resp.check_runs {
                    let Some(id) = cr.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    let raw = serde_json::to_string(&cr)?;
                    if let Some(outcome) = parse_check_run(&raw) {
                        let outcome_label = serde_json::to_string(&outcome).ok();
                        let inserted = self
                            .db
                            .record_observation(
                                repo,
                                "check_run",
                                id,
                                Some(pr.number as i64),
                                &raw,
                                outcome_label.as_deref(),
                            )
                            .await?;
                        if inserted {
                            ledger.apply_check_run(
                                &outcome,
                                Some(pr.number),
                                Some(id as u64),
                                author,
                                "observed_pr",
                            );
                            changed = true;
                        }
                    }
                }
            }

            let comments_url = format!(
                "{}/repos/{repo}/issues/{}/comments?per_page=100",
                self.config.github_api_base, pr.number
            );
            if let Some(val) = self.get_with_etag(&comments_url, token).await?
                && let Some(arr) = val.as_array()
            {
                for c in arr {
                    let Some(id) = c.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    let raw = serde_json::to_string(c)?;
                    if let Some(msg) = parse_issue_comment(&raw) {
                        let outcome_label = serde_json::to_string(&msg).ok();
                        let inserted = self
                            .db
                            .record_observation(
                                repo,
                                "comment",
                                id,
                                Some(pr.number as i64),
                                &raw,
                                outcome_label.as_deref(),
                            )
                            .await?;
                        if inserted {
                            if matches!(msg, BotMessage::Unrecognised { .. }) {
                                warn!(repo, pr = pr.number, comment_id = id, "unrecognised CodeRabbit comment");
                            }
                            ledger.apply_bot_message(&msg);
                            changed = true;
                        }
                    }
                }
            }

            let reviews_url = format!(
                "{}/repos/{repo}/pulls/{}/reviews?per_page=50",
                self.config.github_api_base, pr.number
            );
            if let Some(val) = self.get_with_etag(&reviews_url, token).await?
                && let Some(arr) = val.as_array()
            {
                for r in arr {
                    let Some(id) = r.get("id").and_then(Value::as_i64) else {
                        continue;
                    };
                    let raw = serde_json::to_string(r)?;
                    if let Some(msg) = parse_pr_review(&raw) {
                        let outcome_label = serde_json::to_string(&msg).ok();
                        let inserted = self
                            .db
                            .record_observation(
                                repo,
                                "review",
                                id,
                                Some(pr.number as i64),
                                &raw,
                                outcome_label.as_deref(),
                            )
                            .await?;
                        if inserted {
                            ledger.apply_bot_message(&msg);
                            changed = true;
                        }
                    }
                }
            }
        }

        if changed {
            info!(repo, "updated budget ledger from GitHub observations");
            self.db.save_budget_ledger(repo, &ledger, now).await?;
        }
        Ok(())
    }
}
