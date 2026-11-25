use crate::config::Config;
use crate::findings::{ParsedFinding, parse_outside_diff_findings, parse_review_comment};
use crate::parse::check_run::{CheckRunOutcome, parse_check_run};
use crate::synthetic::{ProactiveBatch, build_base_tree_entries};
use anyhow::{Context, Result};
use jiff::Timestamp;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct OpenedProactivePr {
    pub pr_number: u64,
    pub head_sha: String,
    pub base_branch: String,
    pub head_branch: String,
}

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

    /// Build two synthetic `review/<area>-<ts>/{base,all}` branches via the App (Git Data API)
    /// and open a draft PR + apply `cr-review`/`cr-proactive` labels via the user PAT so CodeRabbit
    /// reviews the exact unreviewed diff.
    pub async fn open_proactive_pr(
        &mut self,
        repo: &str,
        default_branch: &str,
        batch: &ProactiveBatch,
    ) -> Result<OpenedProactivePr> {
        let user_token = self
            .config
            .github_user_token
            .as_deref()
            .context("CR_STEWARD_USER_TOKEN not configured; cannot open proactive PR")?
            .to_string();
        let app_token = self.installation_token_for_repo(repo).await?;

        // 1. Resolve current default-branch commit & tree SHA.
        let ref_url = format!(
            "{}/repos/{repo}/git/ref/heads/{default_branch}",
            self.config.github_api_base
        );
        let ref_val: Value = self
            .client
            .get(&ref_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let default_commit_sha = ref_val
            .get("object")
            .and_then(|o| o.get("sha"))
            .and_then(Value::as_str)
            .context("missing default branch commit sha")?
            .to_string();

        let commit_url = format!(
            "{}/repos/{repo}/git/commits/{default_commit_sha}",
            self.config.github_api_base
        );
        let commit_val: Value = self
            .client
            .get(&commit_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let head_tree_sha = commit_val
            .get("tree")
            .and_then(|t| t.get("sha"))
            .and_then(Value::as_str)
            .context("missing default branch tree sha")?
            .to_string();

        // 2. Read current tree blobs and construct synthetic base tree.
        let tree_url = format!(
            "{}/repos/{repo}/git/trees/{head_tree_sha}?recursive=1",
            self.config.github_api_base
        );
        let tree_val: Value = self
            .client
            .get(&tree_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let head_blobs: Vec<(String, String, String)> = tree_val
            .get("tree")
            .and_then(Value::as_array)
            .context("missing tree array")?
            .iter()
            .filter_map(|e| {
                if e.get("type").and_then(Value::as_str) != Some("blob") {
                    return None;
                }
                Some((
                    e.get("path").and_then(Value::as_str)?.to_string(),
                    e.get("mode").and_then(Value::as_str)?.to_string(),
                    e.get("sha").and_then(Value::as_str)?.to_string(),
                ))
            })
            .collect();

        let base_entries = build_base_tree_entries(&head_blobs, &batch.files);
        let create_tree_url = format!("{}/repos/{repo}/git/trees", self.config.github_api_base);
        let base_tree_resp: Value = self
            .client
            .post(&create_tree_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "tree": base_entries }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let base_tree_sha = base_tree_resp
            .get("sha")
            .and_then(Value::as_str)
            .context("missing created base tree sha")?
            .to_string();

        // 3. Create synthetic base & head commits (unsigned; review/** is outside default branch ruleset).
        let create_commit_url = format!("{}/repos/{repo}/git/commits", self.config.github_api_base);
        let base_commit_resp: Value = self
            .client
            .post(&create_commit_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "message": format!("review({}): base snapshot", batch.area),
                "tree": base_tree_sha,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let base_commit_sha = base_commit_resp
            .get("sha")
            .and_then(Value::as_str)
            .context("missing base commit sha")?
            .to_string();

        let head_commit_resp: Value = self
            .client
            .post(&create_commit_url)
            .header(AUTHORIZATION, format!("Bearer {app_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "message": format!("review({}): unreviewed delta ({} files)", batch.area, batch.files.len()),
                "tree": head_tree_sha,
                "parents": [base_commit_sha],
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let head_commit_sha = head_commit_resp
            .get("sha")
            .and_then(Value::as_str)
            .context("missing head commit sha")?
            .to_string();

        // 4. Push refs/heads/review/<area>-<ts>/{base,all}.
        let safe_area: String = batch
            .area
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let ts = Timestamp::now().as_second();
        let base_branch = format!("review/{safe_area}-{ts}/base");
        let head_branch = format!("review/{safe_area}-{ts}/all");
        let refs_url = format!("{}/repos/{repo}/git/refs", self.config.github_api_base);
        for (branch, sha) in [(&base_branch, &base_commit_sha), (&head_branch, &head_commit_sha)] {
            self.client
                .post(&refs_url)
                .header(AUTHORIZATION, format!("Bearer {app_token}"))
                .header(USER_AGENT, "coderabbit-steward/0.1")
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({
                    "ref": format!("refs/heads/{branch}"),
                    "sha": sha,
                }))
                .send()
                .await?
                .error_for_status()?;
        }

        // 5. Open draft PR and apply labels via user PAT so CodeRabbit reviews it.
        let pr_url = format!("{}/repos/{repo}/pulls", self.config.github_api_base);
        let pr_resp: Value = self
            .client
            .post(&pr_url)
            .header(AUTHORIZATION, format!("Bearer {user_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "title": format!("[review only] proactive: {}", batch.area),
                "head": head_branch,
                "base": base_branch,
                "draft": true,
                "body": format!(
                    "Automated proactive review of {} unreviewed file(s) (~{} lines) in `{}`.\n\nWill be closed automatically once CodeRabbit finishes.",
                    batch.files.len(),
                    batch.total_lines,
                    batch.area
                ),
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let pr_number = pr_resp
            .get("number")
            .and_then(Value::as_u64)
            .context("missing opened PR number")?;

        let labels_url = format!(
            "{}/repos/{repo}/issues/{pr_number}/labels",
            self.config.github_api_base
        );
        self.client
            .post(&labels_url)
            .header(AUTHORIZATION, format!("Bearer {user_token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "labels": ["cr-review", "cr-proactive"] }))
            .send()
            .await?
            .error_for_status()?;

        Ok(OpenedProactivePr {
            pr_number,
            head_sha: head_commit_sha,
            base_branch,
            head_branch,
        })
    }

    /// Check whether a PR's head commit has a finished CodeRabbit check run.
    pub async fn pr_coderabbit_outcome(&mut self, repo: &str, head_sha: &str) -> Result<Option<CheckRunOutcome>> {
        let token = self.read_token_for_repo(repo).await?;
        let url = format!(
            "{}/repos/{repo}/commits/{head_sha}/check-runs?per_page=50",
            self.config.github_api_base
        );
        let resp: Value = self
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
        let runs = resp.get("check_runs").and_then(Value::as_array);
        for cr in runs.into_iter().flatten() {
            if let Ok(raw) = serde_json::to_string(cr)
                && let Some(outcome) = parse_check_run(&raw)
            {
                return Ok(Some(outcome));
            }
        }
        Ok(None)
    }

    /// Collect inline review comments and outside-diff findings from a CodeRabbit-reviewed PR.
    pub async fn fetch_pr_findings(&mut self, repo: &str, pr_number: u64) -> Result<Vec<ParsedFinding>> {
        let token = self.read_token_for_repo(repo).await?;
        let mut out = Vec::new();

        let comments_url = format!(
            "{}/repos/{repo}/pulls/{pr_number}/comments?per_page=100",
            self.config.github_api_base
        );
        let comments: Vec<Value> = self
            .client
            .get(&comments_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for c in &comments {
            if let Some(f) = parse_review_comment(c) {
                out.push(f);
            }
        }

        let reviews_url = format!(
            "{}/repos/{repo}/pulls/{pr_number}/reviews?per_page=50",
            self.config.github_api_base
        );
        let reviews: Vec<Value> = self
            .client
            .get(&reviews_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for r in &reviews {
            if let Some(body) = r.get("body").and_then(Value::as_str) {
                out.extend(parse_outside_diff_findings(body));
            }
        }
        Ok(out)
    }

    /// Close a proactive review PR and delete its temporary `review/**` branches via the GitHub App.
    pub async fn close_proactive_pr_and_cleanup(
        &mut self,
        repo: &str,
        pr_number: u64,
        summary_comment: &str,
    ) -> Result<()> {
        let token = self.installation_token_for_repo(repo).await?;

        let pr_url = format!("{}/repos/{repo}/pulls/{pr_number}", self.config.github_api_base);
        let pr_val: Value = self
            .client
            .get(&pr_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let comment_url = format!(
            "{}/repos/{repo}/issues/{pr_number}/comments",
            self.config.github_api_base
        );
        let _ = self
            .client
            .post(&comment_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "body": summary_comment }))
            .send()
            .await;

        self.client
            .patch(&pr_url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await?
            .error_for_status()?;

        for field in ["head", "base"] {
            if let Some(branch) = pr_val
                .get(field)
                .and_then(|o| o.get("ref"))
                .and_then(Value::as_str)
                && branch.starts_with("review/")
            {
                let ref_url = format!(
                    "{}/repos/{repo}/git/refs/heads/{branch}",
                    self.config.github_api_base
                );
                let _ = self
                    .client
                    .delete(&ref_url)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(USER_AGENT, "coderabbit-steward/0.1")
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await;
            }
        }
        Ok(())
    }

    /// Create, update, or close the rolling `CodeRabbit findings` (`cr-findings`) issue for a repo.
    pub async fn sync_findings_issue(
        &mut self,
        repo: &str,
        existing_issue: Option<i64>,
        body: &str,
        has_open_findings: bool,
    ) -> Result<Option<i64>> {
        let token = self.installation_token_for_repo(repo).await?;
        if let Some(num) = existing_issue {
            let url = format!("{}/repos/{repo}/issues/{num}", self.config.github_api_base);
            let state = if has_open_findings { "open" } else { "closed" };
            self.client
                .patch(&url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(USER_AGENT, "coderabbit-steward/0.1")
                .header("Accept", "application/vnd.github+json")
                .json(&serde_json::json!({ "body": body, "state": state }))
                .send()
                .await?
                .error_for_status()?;
            return Ok(Some(num));
        }

        if !has_open_findings {
            return Ok(None);
        }

        let url = format!("{}/repos/{repo}/issues", self.config.github_api_base);
        let resp: Value = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(USER_AGENT, "coderabbit-steward/0.1")
            .header("Accept", "application/vnd.github+json")
            .json(&serde_json::json!({
                "title": "CodeRabbit findings",
                "body": body,
                "labels": ["cr-findings"],
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.get("number").and_then(Value::as_i64))
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
