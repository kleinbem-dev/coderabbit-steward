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
use reqwest::header::{ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
    fork: Option<bool>,
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
            .user_agent("coderabbit-steward/0.1")
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

            let mut seen_names: HashSet<String> = HashSet::new();
            let mut listing_complete = true;
            let mut next_url: Option<String> = Some(format!(
                "{}/users/{owner}/repos?per_page=100&type=owner",
                self.config.github_api_base
            ));
            while let Some(url) = next_url.take() {
                let resp = self
                    .client
                    .get(&url)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    warn!(owner, url = %url, status = %resp.status(), "repo discovery page failed");
                    listing_complete = false;
                    break;
                }
                next_url = parse_next_link(
                    resp.headers()
                        .get("link")
                        .and_then(|v| v.to_str().ok()),
                );
                let repos: Vec<GhRepo> = match resp.json().await {
                    Ok(r) => r,
                    Err(err) => {
                        warn!(owner, error = %err, "failed to decode repo discovery page");
                        listing_complete = false;
                        break;
                    }
                };
                for repo in repos {
                    seen_names.insert(repo.name.to_ascii_lowercase());
                    if repo.archived.unwrap_or(false)
                        || repo.is_private
                        || repo.fork.unwrap_or(false)
                    {
                        if self.db.delete_repo(&repo.owner.login, &repo.name).await? {
                            info!(
                                repo = %format!("{}/{}", repo.owner.login, repo.name),
                                archived = repo.archived.unwrap_or(false),
                                private = repo.is_private,
                                fork = repo.fork.unwrap_or(false),
                                "removed excluded repository from tracking"
                            );
                        }
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

            if listing_complete {
                let existing = self.db.list_repos().await?;
                for repo in existing
                    .into_iter()
                    .filter(|r| r.owner.eq_ignore_ascii_case(&owner))
                {
                    if seen_names.contains(&repo.name.to_ascii_lowercase()) {
                        continue;
                    }
                    let check_url = format!(
                        "{}/repos/{}/{}",
                        self.config.github_api_base, repo.owner, repo.name
                    );
                    let resp = self
                        .client
                        .get(&check_url)
                        .header(AUTHORIZATION, format!("Bearer {token}"))
                        .header(ACCEPT, "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    let should_purge = if resp.status() == reqwest::StatusCode::NOT_FOUND {
                        true
                    } else if resp.status().is_success() {
                        resp.json::<GhRepo>().await.ok().is_some_and(|r| {
                            r.is_private || r.archived.unwrap_or(false) || r.fork.unwrap_or(false)
                        })
                    } else {
                        false
                    };
                    if should_purge && self.db.delete_repo(&repo.owner, &repo.name).await? {
                        info!(
                            repo = %format!("{}/{}", repo.owner, repo.name),
                            "removed inaccessible or newly excluded repository from tracking"
                        );
                    }
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

    /// Fetch a URL using stored ETag without committing the new ETag until the caller succeeds.
    async fn get_with_deferred_etag(
        &self,
        url: &str,
        token: &str,
    ) -> Result<Option<(Value, Option<String>)>> {
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
        Ok(Some((val, new_etag)))
    }

    fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
        let link = headers.get("link")?.to_str().ok()?;
        for part in link.split(',') {
            let mut segs = part.trim().split(';');
            let url_part = segs.next()?.trim();
            let is_next = segs.any(|s| s.trim() == r#"rel="next""#);
            if is_next && url_part.starts_with('<') && url_part.ends_with('>') {
                return Some(url_part[1..url_part.len() - 1].to_string());
            }
        }
        None
    }

    async fn get_paginated_array(&self, first_url: &str, token: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut next = Some(first_url.to_string());
        while let Some(url) = next {
            let resp = self
                .client
                .get(&url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(USER_AGENT, "coderabbit-steward/0.1")
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()?;
            next = Self::parse_next_link(resp.headers());
            let page: Value = resp.json().await?;
            if let Some(arr) = page.as_array() {
                out.extend(arr.iter().cloned());
            }
        }
        Ok(out)
    }

    async fn fetch_commit_with_all_files(&self, repo: &str, sha: &str, token: &str) -> Result<(Timestamp, Vec<Value>)> {
        let mut next = Some(format!(
            "{}/repos/{repo}/commits/{sha}?per_page=100",
            self.config.github_api_base
        ));
        let mut commit_ts: Option<Timestamp> = None;
        let mut files = Vec::new();

        while let Some(url) = next {
            let resp = self
                .client
                .get(&url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(USER_AGENT, "coderabbit-steward/0.1")
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?
                .error_for_status()?;
            next = Self::parse_next_link(resp.headers());
            let val: Value = resp.json().await?;
            if commit_ts.is_none() {
                commit_ts = val
                    .get("commit")
                    .and_then(|c| c.get("committer"))
                    .and_then(|c| c.get("date"))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<Timestamp>().ok());
            }
            if let Some(arr) = val.get("files").and_then(Value::as_array) {
                files.extend(arr.iter().cloned());
            }
        }
        Ok((commit_ts.unwrap_or_else(Timestamp::now), files))
    }

    /// Fetch the full default-branch blob map (`path -> blob_sha`), falling back to iterative
    /// subtree traversal if GitHub truncates the recursive tree response.
    async fn fetch_full_tree_blobs(
        &self,
        repo: &str,
        default_branch: &str,
        token: &str,
    ) -> Result<HashMap<String, String>> {
        let tree_url = format!(
            "{}/repos/{repo}/git/trees/{default_branch}?recursive=1",
            self.config.github_api_base
        );
        let tree_val = self.get_json(&tree_url, token).await?;
        let truncated = tree_val
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !truncated {
            let entries = tree_val
                .get("tree")
                .and_then(Value::as_array)
                .context("tree response missing tree array")?;
            let mut blobs = HashMap::new();
            for entry in entries {
                if entry.get("type").and_then(Value::as_str) == Some("blob")
                    && let (Some(path), Some(sha)) = (
                        entry.get("path").and_then(Value::as_str),
                        entry.get("sha").and_then(Value::as_str),
                    )
                {
                    blobs.insert(path.to_string(), sha.to_string());
                }
            }
            return Ok(blobs);
        }

        // Fallback for truncated trees: walk subtrees iteratively without recursion.
        let mut blobs = HashMap::new();
        let mut stack: Vec<(String, String)> = vec![(String::new(), default_branch.to_string())];
        while let Some((prefix, tree_ref)) = stack.pop() {
            let url = format!(
                "{}/repos/{repo}/git/trees/{tree_ref}",
                self.config.github_api_base
            );
            let val = self.get_json(&url, token).await?;
            let entries = val
                .get("tree")
                .and_then(Value::as_array)
                .context("subtree response missing tree array")?;
            for entry in entries {
                let Some(name) = entry.get("path").and_then(Value::as_str) else {
                    continue;
                };
                let Some(sha) = entry.get("sha").and_then(Value::as_str) else {
                    continue;
                };
                let full_path = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                match entry.get("type").and_then(Value::as_str) {
                    Some("blob") => {
                        blobs.insert(full_path, sha.to_string());
                    }
                    Some("tree") => {
                        stack.push((full_path, sha.to_string()));
                    }
                    _ => {}
                }
            }
        }
        Ok(blobs)
    }

    async fn check_run_review_status(
        &self,
        repo: &str,
        sha: &str,
        token: &str,
    ) -> Result<ReviewCheckStatus> {
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/check-runs?per_page=50",
            self.config.github_api_base
        );
        let val = self.get_json(&url, token).await?;
        let resp: GhCheckRunsResp = serde_json::from_value(val)?;
        let mut pending = false;
        for cr in &resp.check_runs {
            if let Ok(raw) = serde_json::to_string(cr)
                && let Some(outcome) = parse_check_run(&raw)
            {
                match outcome {
                    CheckRunOutcome::Reviewed { .. } => return Ok(ReviewCheckStatus::Reviewed),
                    CheckRunOutcome::InProgress { .. } => pending = true,
                    _ => {}
                }
            }
        }
        if pending {
            Ok(ReviewCheckStatus::Pending)
        } else {
            Ok(ReviewCheckStatus::NotReviewed)
        }
    }

    /// Walk new default-branch commits (capped at 20 per poll tick) and update `file_coverage`.
    async fn scan_repo_coverage(&self, repo: &str, default_branch: &str, token: &str) -> Result<()> {
        let cursor = self.db.get_scan_cursor(repo).await?;
        let existing = self.db.list_file_coverage(repo).await?;
        let mut map: HashMap<String, FileCoverageState> = existing
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect();

        // First scan: require a complete default-branch tree before advancing any cursor.
        let initial_tree_blobs: Option<HashMap<String, String>> = if cursor.is_none() {
            let blobs = self
                .fetch_full_tree_blobs(repo, default_branch, token)
                .await?;
            let now_str = Timestamp::now().to_string();
            for path in blobs.keys() {
                let excluded = is_excluded_path(path);
                map.entry(path.clone()).or_insert(FileCoverageState {
                    path: path.clone(),
                    reviewed_blob: None,
                    reviewed_commit: None,
                    by: None,
                    unreviewed_lines: if excluded { 0 } else { 1 },
                    unreviewed_since: if excluded { None } else { Some(now_str.clone()) },
                    excluded,
                });
            }
            Some(blobs)
        } else {
            None
        };

        // Collect commits to process in chronological (oldest-first) order with deferred ETag.
        let (etag_url, pending_etag, commit_shas): (String, Option<String>, Vec<String>) =
            match cursor.as_deref() {
                None => {
                    let commits_url = format!(
                        "{}/repos/{repo}/commits?sha={default_branch}&per_page=20",
                        self.config.github_api_base
                    );
                    let Some((Value::Array(arr), etag)) =
                        self.get_with_deferred_etag(&commits_url, token).await?
                    else {
                        return Ok(());
                    };
                    let shas = arr
                        .into_iter()
                        .rev()
                        .filter_map(|c| {
                            c.get("sha").and_then(Value::as_str).map(ToString::to_string)
                        })
                        .collect();
                    (commits_url, etag, shas)
                }
                Some(base_sha) => {
                    let cmp_url = format!(
                        "{}/repos/{repo}/compare/{base_sha}...{default_branch}",
                        self.config.github_api_base
                    );
                    let Some((cmp_val, etag)) =
                        self.get_with_deferred_etag(&cmp_url, token).await?
                    else {
                        return Ok(());
                    };
                    let shas = cmp_val
                        .get("commits")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .take(20)
                                .filter_map(|c| {
                                    c.get("sha").and_then(Value::as_str).map(ToString::to_string)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (cmp_url, etag, shas)
                }
            };

        if commit_shas.is_empty() && cursor.is_some() {
            if let Some(etag) = pending_etag {
                self.db.set_etag(&etag_url, &etag).await?;
            }
            return Ok(());
        }

        let mut removed_paths: Vec<String> = Vec::new();
        let mut last_sha: Option<String> = None;
        let mut completed_all = true;

        for sha in &commit_shas {
            let Ok((commit_ts, raw_files)) =
                self.fetch_commit_with_all_files(repo, sha, token).await
            else {
                completed_all = false;
                break;
            };

            let pulls_url = format!(
                "{}/repos/{repo}/commits/{sha}/pulls",
                self.config.github_api_base
            );
            let Ok(Value::Array(prs)) = self.get_json(&pulls_url, token).await else {
                completed_all = false;
                break;
            };

            let mut reviewed = false;
            let mut pending_or_err = false;
            for pr in prs {
                if let Some(head_sha) = pr
                    .get("head")
                    .and_then(|h| h.get("sha"))
                    .and_then(Value::as_str)
                {
                    match self.check_run_review_status(repo, head_sha, token).await {
                        Ok(ReviewCheckStatus::Reviewed) => {
                            reviewed = true;
                            break;
                        }
                        Ok(ReviewCheckStatus::Pending) | Err(_) => {
                            pending_or_err = true;
                        }
                        Ok(ReviewCheckStatus::NotReviewed) => {}
                    }
                }
            }
            // Stop advancing the cursor before a commit whose CodeRabbit check is still in progress
            // or whose verdict could not be fetched, so the next tick reconciles it cleanly.
            if !reviewed && pending_or_err {
                completed_all = false;
                break;
            }

            let mut changes = Vec::new();
            for f in &raw_files {
                let Some(path) = f.get("filename").and_then(Value::as_str) else {
                    continue;
                };
                let status = f.get("status").and_then(Value::as_str).unwrap_or("");
                if status == "renamed"
                    && let Some(prev) = f.get("previous_filename").and_then(Value::as_str)
                {
                    removed_paths.push(prev.to_string());
                    changes.push(CommitFileChange {
                        path: prev.to_string(),
                        blob_sha: None,
                        lines_changed: 0,
                        removed: true,
                    });
                }
                let removed = status == "removed";
                if removed {
                    removed_paths.push(path.to_string());
                }
                let additions = f.get("additions").and_then(Value::as_u64).unwrap_or(0);
                let deletions = f.get("deletions").and_then(Value::as_u64).unwrap_or(0);
                changes.push(CommitFileChange {
                    path: path.to_string(),
                    blob_sha: f.get("sha").and_then(Value::as_str).map(ToString::to_string),
                    lines_changed: (additions + deletions) as u32,
                    removed,
                });
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

        // After replaying initial history, apply `[review only]` baseline PRs only for files
        // currently on the default branch whose blob SHA matches the reviewed baseline blob.
        if let Some(current_blobs) = &initial_tree_blobs
            && last_sha.is_some()
        {
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
                    if !matches!(
                        self.check_run_review_status(repo, head_sha, token).await,
                        Ok(ReviewCheckStatus::Reviewed)
                    ) {
                        continue;
                    }
                    let files_url = format!(
                        "{}/repos/{repo}/pulls/{pr_num}/files?per_page=100",
                        self.config.github_api_base
                    );
                    if let Ok(files) = self.get_paginated_array(&files_url, token).await {
                        let matching: Vec<CommitFileChange> = files
                            .iter()
                            .filter_map(|f| {
                                let path = f.get("filename").and_then(Value::as_str)?;
                                let blob = f.get("sha").and_then(Value::as_str)?;
                                if current_blobs.get(path).map(String::as_str) != Some(blob) {
                                    return None;
                                }
                                Some(CommitFileChange {
                                    path: path.to_string(),
                                    blob_sha: Some(blob.to_string()),
                                    lines_changed: 0,
                                    removed: false,
                                })
                            })
                            .collect();
                        apply_commit_changes(
                            &mut map,
                            &matching,
                            &CommitVerdict::Reviewed {
                                by: "full".to_string(),
                                commit_sha: head_sha.to_string(),
                            },
                        );
                    }
                }
            }
        }

        if last_sha.is_none() && initial_tree_blobs.is_none() {
            return Ok(());
        }

        let deleted: Vec<String> = removed_paths
            .into_iter()
            .filter(|p| !map.contains_key(p))
            .collect();
        let upserts: Vec<FileCoverageState> = map.into_values().collect();
        self.db
            .commit_coverage_batch(repo, &deleted, &upserts, last_sha.as_deref())
            .await?;

        if completed_all
            && let Some(etag) = pending_etag
        {
            self.db.set_etag(&etag_url, &etag).await?;
        }
        if let Some(sha) = last_sha {
            info!(repo, scanned = commit_shas.len(), cursor = %sha, "updated coverage map");
        }
        Ok(())
    }
}

enum ReviewCheckStatus {
    Reviewed,
    Pending,
    NotReviewed,
}

fn parse_next_link(header: Option<&str>) -> Option<String> {
    let header = header?;
    for part in header.split(',') {
        let mut segments = part.split(';').map(str::trim);
        let url_part = segments.next()?;
        if segments.any(|s| s == r#"rel="next""#)
            && let Some(url) = url_part.strip_prefix('<').and_then(|u| u.strip_suffix('>'))
        {
            return Some(url.to_string());
        }
    }
    None
}
