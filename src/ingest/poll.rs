use crate::config::Config;
use crate::coverage::{
    CommitFileChange, CommitVerdict, FileCoverageState, apply_commit_changes, is_excluded_path,
};
use crate::db::Db;
use crate::parse::check_run::{CheckRunOutcome, parse_check_run};
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
                if let Err(err) = self
                    .scan_repo_coverage(&full, &repo.default_branch, &token)
                    .await
                {
                    warn!(repo = full, error = %err, "scan_repo_coverage failed");
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

    async fn get_json(&self, url: &str, token: &str) -> Result<Value> {
        let val = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(val)
    }

    async fn commit_has_coderabbit_review(&self, repo: &str, sha: &str, token: &str) -> bool {
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/check-runs?per_page=50",
            self.config.github_api_base
        );
        let Ok(val) = self.get_json(&url, token).await else {
            return false;
        };
        let Ok(resp) = serde_json::from_value::<GhCheckRunsResp>(val) else {
            return false;
        };
        resp.check_runs.iter().any(|cr| {
            serde_json::to_string(cr)
                .ok()
                .and_then(|raw| parse_check_run(&raw))
                .is_some_and(|o| matches!(o, CheckRunOutcome::Reviewed { .. }))
        })
    }

    /// Walk new default-branch commits (capped at 20 per poll tick) and update `file_coverage`.
    async fn scan_repo_coverage(&self, repo: &str, default_branch: &str, token: &str) -> Result<()> {
        let cursor = self.db.get_scan_cursor(repo).await?;
        let existing = self.db.list_file_coverage(repo).await?;
        let mut map: HashMap<String, FileCoverageState> = existing
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();

        // First scan: seed default-branch tree + import closed `[review only]` baseline PRs.
        if cursor.is_none() {
            let tree_url = format!(
                "{}/repos/{repo}/git/trees/{default_branch}?recursive=1",
                self.config.github_api_base
            );
            if let Ok(tree_val) = self.get_json(&tree_url, token).await
                && let Some(entries) = tree_val.get("tree").and_then(Value::as_array)
            {
                let now_str = Timestamp::now().to_string();
                for entry in entries {
                    if entry.get("type").and_then(Value::as_str) != Some("blob") {
                        continue;
                    }
                    let Some(path) = entry.get("path").and_then(Value::as_str) else {
                        continue;
                    };
                    let excluded = is_excluded_path(path);
                    map.entry(path.to_string()).or_insert(FileCoverageState {
                        path: path.to_string(),
                        reviewed_blob: None,
                        reviewed_commit: None,
                        by: None,
                        unreviewed_lines: if excluded { 0 } else { 1 },
                        unreviewed_since: if excluded { None } else { Some(now_str.clone()) },
                        excluded,
                    });
                }
            }

            // Import closed `[review only]` baseline PRs if CodeRabbit reviewed them.
            let closed_url = format!(
                "{}/repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page=20",
                self.config.github_api_base
            );
            if let Ok(Value::Array(prs)) = self.get_json(&closed_url, token).await {
                for pr in prs {
                    let title = pr.get("title").and_then(Value::as_str).unwrap_or("");
                    if !title.starts_with("[review only]") {
                        continue;
                    }
                    let Some(pr_num) = pr.get("number").and_then(Value::as_u64) else {
                        continue;
                    };
                    let Some(head_sha) = pr
                        .get("head")
                        .and_then(|h| h.get("sha"))
                        .and_then(Value::as_str)
                    else {
                        continue;
                    };
                    if !self.commit_has_coderabbit_review(repo, head_sha, token).await {
                        continue;
                    }
                    let files_url = format!(
                        "{}/repos/{repo}/pulls/{pr_num}/files?per_page=100",
                        self.config.github_api_base
                    );
                    if let Ok(Value::Array(files)) = self.get_json(&files_url, token).await {
                        let changes: Vec<CommitFileChange> = files
                            .iter()
                            .filter_map(|f| {
                                let path = f.get("filename").and_then(Value::as_str)?.to_string();
                                Some(CommitFileChange {
                                    path,
                                    blob_sha: f.get("sha").and_then(Value::as_str).map(ToString::to_string),
                                    lines_changed: f
                                        .get("changes")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0) as u32,
                                    removed: f.get("status").and_then(Value::as_str) == Some("removed"),
                                })
                            })
                            .collect();
                        apply_commit_changes(
                            &mut map,
                            &changes,
                            &CommitVerdict::Reviewed {
                                by: "full".to_string(),
                                commit_sha: head_sha.to_string(),
                            },
                        );
                    }
                }
            }
        }

        // Collect commits to process in chronological (oldest-first) order.
        let commit_shas: Vec<String> = match cursor.as_deref() {
            None => {
                let commits_url = format!(
                    "{}/repos/{repo}/commits?sha={default_branch}&per_page=20",
                    self.config.github_api_base
                );
                let Some(Value::Array(arr)) = self.get_with_etag(&commits_url, token).await? else {
                    return Ok(());
                };
                arr.into_iter()
                    .rev()
                    .filter_map(|c| c.get("sha").and_then(Value::as_str).map(ToString::to_string))
                    .collect()
            }
            Some(base_sha) => {
                let cmp_url = format!(
                    "{}/repos/{repo}/compare/{base_sha}...{default_branch}",
                    self.config.github_api_base
                );
                let Some(cmp_val) = self.get_with_etag(&cmp_url, token).await? else {
                    return Ok(());
                };
                cmp_val
                    .get("commits")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .take(20)
                            .filter_map(|c| c.get("sha").and_then(Value::as_str).map(ToString::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };

        if commit_shas.is_empty() && cursor.is_some() {
            return Ok(());
        }

        let mut removed_paths: Vec<String> = Vec::new();
        let mut last_sha: Option<String> = None;

        for sha in &commit_shas {
            let commit_url = format!("{}/repos/{repo}/commits/{sha}", self.config.github_api_base);
            let Ok(commit_val) = self.get_json(&commit_url, token).await else {
                break;
            };
            let commit_ts = commit_val
                .get("commit")
                .and_then(|c| c.get("committer"))
                .and_then(|c| c.get("date"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<Timestamp>().ok())
                .unwrap_or_else(Timestamp::now);

            let changes: Vec<CommitFileChange> = commit_val
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| {
                            let path = f.get("filename").and_then(Value::as_str)?.to_string();
                            let removed = f.get("status").and_then(Value::as_str) == Some("removed");
                            if removed {
                                removed_paths.push(path.clone());
                            }
                            let additions = f.get("additions").and_then(Value::as_u64).unwrap_or(0);
                            let deletions = f.get("deletions").and_then(Value::as_u64).unwrap_or(0);
                            Some(CommitFileChange {
                                path,
                                blob_sha: f.get("sha").and_then(Value::as_str).map(ToString::to_string),
                                lines_changed: (additions + deletions) as u32,
                                removed,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            let mut reviewed = false;
            let pulls_url = format!(
                "{}/repos/{repo}/commits/{sha}/pulls",
                self.config.github_api_base
            );
            if let Ok(Value::Array(prs)) = self.get_json(&pulls_url, token).await {
                for pr in prs {
                    if let Some(head_sha) = pr
                        .get("head")
                        .and_then(|h| h.get("sha"))
                        .and_then(Value::as_str)
                        && self.commit_has_coderabbit_review(repo, head_sha, token).await
                    {
                        reviewed = true;
                        break;
                    }
                }
            }

            let verdict = if reviewed {
                CommitVerdict::Reviewed {
                    by: "pr".to_string(),
                    commit_sha: sha.clone(),
                }
            } else {
                CommitVerdict::Unreviewed { at: commit_ts }
            };

            apply_commit_changes(&mut map, &changes, &verdict);
            last_sha = Some(sha.clone());
        }

        for path in removed_paths {
            if !map.contains_key(&path) {
                self.db.delete_file_coverage(repo, &path).await?;
            }
        }
        for state in map.values() {
            self.db.upsert_file_coverage(repo, state).await?;
        }
        if let Some(sha) = last_sha {
            self.db.set_scan_cursor(repo, &sha).await?;
            info!(repo, scanned = commit_shas.len(), cursor = %sha, "updated coverage map");
        }
        Ok(())
    }
}
