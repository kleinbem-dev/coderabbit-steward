use crate::budget::{BudgetLedger, Confidence, SpendRecord, SpendState};
use crate::config::Mode;
use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::str::FromStr;

pub fn now_rfc3339() -> String {
    Timestamp::now().to_string()
}

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RepoRow {
    pub id: i64,
    pub owner: String,
    pub name: String,
    pub visibility: String,
    pub stars: i64,
    pub default_branch: String,
    pub has_coderabbit: i64,
    pub file_cap: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct BudgetStateRow {
    pub repo: String,
    pub pool: String,
    pub limit_per_hour: i64,
    pub refill_minutes: i64,
    pub remaining: i64,
    pub next_free_at: Option<String>,
    pub confidence: String,
    pub fleet_cooldown_until: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SpendRow {
    pub id: i64,
    pub repo: String,
    pub identity: String,
    pub at: String,
    pub cause: String,
    pub pr: Option<i64>,
    pub check_run_id: Option<i64>,
    pub state: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct QueueRow {
    pub id: i64,
    pub repo: String,
    pub kind: String,
    pub pr_number: Option<i64>,
    pub paths_json: Option<String>,
    pub priority: i64,
    pub state: String,
    pub not_before: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FindingRow {
    pub id: i64,
    pub repo: String,
    pub path: String,
    pub line: Option<i64>,
    pub commit_sha: Option<String>,
    pub severity: String,
    pub category: String,
    pub title: String,
    pub body: String,
    pub fingerprint: String,
    pub status: String,
    pub review_run: Option<i64>,
    pub issue_number: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SettingsRow {
    pub id: i64,
    pub mode: String,
    pub repo_daily_cap: i64,
    pub fleet_daily_cap: i64,
    pub quiet_minutes: i64,
    pub pooled: i64,
    pub updated_at: String,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::from_str(database_url)
            .with_context(|| format!("invalid sqlite URL: {database_url}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .context("failed to connect to sqlite")?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to run migrations")?;
        Ok(Self { pool })
    }

    pub async fn upsert_repo(
        &self,
        owner: &str,
        name: &str,
        visibility: &str,
        stars: i64,
        default_branch: &str,
    ) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO repos (owner, name, visibility, stars, default_branch, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(owner, name) DO UPDATE SET
               visibility = excluded.visibility,
               stars = excluded.stars,
               default_branch = excluded.default_branch,
               updated_at = excluded.updated_at",
        )
        .bind(owner)
        .bind(name)
        .bind(visibility)
        .bind(stars)
        .bind(default_branch)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let full_name = format!("{owner}/{name}");
        sqlx::query(
            "INSERT OR IGNORE INTO budget_state (repo, updated_at) VALUES (?1, ?2)",
        )
        .bind(&full_name)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_repos(&self) -> Result<Vec<RepoRow>> {
        let rows = sqlx::query_as::<_, RepoRow>(
            "SELECT id, owner, name, visibility, stars, default_branch, has_coderabbit, file_cap, updated_at
             FROM repos ORDER BY owner, name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn record_observation(
        &self,
        repo: &str,
        kind: &str,
        github_id: i64,
        pr_number: Option<i64>,
        payload: &str,
        parse_outcome: Option<&str>,
    ) -> Result<bool> {
        let now = now_rfc3339();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO observations (repo, kind, github_id, pr_number, payload, seen_at, parsed, parse_outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .bind(repo)
        .bind(kind)
        .bind(github_id)
        .bind(pr_number)
        .bind(payload)
        .bind(&now)
        .bind(if parse_outcome.is_some() { 1 } else { 0 })
        .bind(parse_outcome)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn load_budget_ledger(&self, repo: &str) -> Result<BudgetLedger> {
        let state = sqlx::query_as::<_, BudgetStateRow>(
            "SELECT repo, pool, limit_per_hour, refill_minutes, remaining, next_free_at, confidence, fleet_cooldown_until, updated_at
             FROM budget_state WHERE repo = ?1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;

        let spends_rows = sqlx::query_as::<_, SpendRow>(
            "SELECT id, repo, identity, at, cause, pr, check_run_id, state, created_at
             FROM spends WHERE repo = ?1 ORDER BY at DESC LIMIT 50",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        let mut ledger = BudgetLedger::default();
        if let Some(s) = state {
            ledger.limit = u32::try_from(s.limit_per_hour.max(1)).unwrap_or(1);
            ledger.refill_minutes = u32::try_from(s.refill_minutes.max(60)).unwrap_or(60);
            ledger.next_free_at = s.next_free_at.as_deref().and_then(|v| v.parse().ok());
            ledger.fleet_cooldown_until = s
                .fleet_cooldown_until
                .as_deref()
                .and_then(|v| v.parse().ok());
            ledger.confidence = Confidence::parse(&s.confidence);
            ledger.last_observed_at = s.updated_at.parse().ok();
        }
        for sr in spends_rows {
            if let Ok(at) = sr.at.parse::<Timestamp>() {
                ledger.spends.push(SpendRecord {
                    at,
                    state: if sr.state == "confirmed" {
                        SpendState::Confirmed
                    } else {
                        SpendState::Provisional
                    },
                    cause: sr.cause,
                    identity: sr.identity,
                    pr: sr.pr.and_then(|v| u64::try_from(v).ok()),
                    check_run_id: sr.check_run_id.and_then(|v| u64::try_from(v).ok()),
                });
            }
        }
        Ok(ledger)
    }

    pub async fn save_budget_ledger(&self, repo: &str, ledger: &BudgetLedger, now: Timestamp) -> Result<()> {
        let snap = ledger.snapshot(now);
        let now_str = now.to_string();
        sqlx::query(
            "INSERT INTO budget_state (repo, pool, limit_per_hour, refill_minutes, remaining, next_free_at, confidence, fleet_cooldown_until, updated_at)
             VALUES (?1, 'pooled', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(repo) DO UPDATE SET
               limit_per_hour = excluded.limit_per_hour,
               refill_minutes = excluded.refill_minutes,
               remaining = excluded.remaining,
               next_free_at = excluded.next_free_at,
               confidence = excluded.confidence,
               fleet_cooldown_until = excluded.fleet_cooldown_until,
               updated_at = excluded.updated_at",
        )
        .bind(repo)
        .bind(i64::from(snap.limit))
        .bind(i64::from(snap.refill_minutes))
        .bind(i64::from(snap.remaining))
        .bind(snap.next_free_at.map(|t| t.to_string()))
        .bind(snap.confidence.as_str())
        .bind(snap.fleet_cooldown_until.map(|t| t.to_string()))
        .bind(&now_str)
        .execute(&self.pool)
        .await?;

        for sp in &ledger.spends {
            let at_str = sp.at.to_string();
            let exists: Option<(i64, String)> = sqlx::query_as(
                "SELECT id, state FROM spends WHERE repo = ?1 AND at = ?2 LIMIT 1",
            )
            .bind(repo)
            .bind(&at_str)
            .fetch_optional(&self.pool)
            .await?;

            match exists {
                Some((id, cur_state)) if cur_state != sp.state.as_str() => {
                    sqlx::query("UPDATE spends SET state = ?1 WHERE id = ?2")
                        .bind(sp.state.as_str())
                        .bind(id)
                        .execute(&self.pool)
                        .await?;
                }
                None => {
                    sqlx::query(
                        "INSERT INTO spends (repo, identity, at, cause, pr, check_run_id, state, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )
                    .bind(repo)
                    .bind(&sp.identity)
                    .bind(&at_str)
                    .bind(&sp.cause)
                    .bind(sp.pr.and_then(|v| i64::try_from(v).ok()))
                    .bind(sp.check_run_id.and_then(|v| i64::try_from(v).ok()))
                    .bind(sp.state.as_str())
                    .bind(&now_str)
                    .execute(&self.pool)
                    .await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub async fn list_spends(&self, repo: &str, limit: i64) -> Result<Vec<SpendRow>> {
        let rows = sqlx::query_as::<_, SpendRow>(
            "SELECT id, repo, identity, at, cause, pr, check_run_id, state, created_at
             FROM spends WHERE repo = ?1 ORDER BY at DESC LIMIT ?2",
        )
        .bind(repo)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn list_budget_states(&self) -> Result<Vec<BudgetStateRow>> {
        let rows = sqlx::query_as::<_, BudgetStateRow>(
            "SELECT repo, pool, limit_per_hour, refill_minutes, remaining, next_free_at, confidence, fleet_cooldown_until, updated_at
             FROM budget_state ORDER BY repo",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_budget_state(&self, repo: &str) -> Result<Option<BudgetStateRow>> {
        let row = sqlx::query_as::<_, BudgetStateRow>(
            "SELECT repo, pool, limit_per_hour, refill_minutes, remaining, next_free_at, confidence, fleet_cooldown_until, updated_at
             FROM budget_state WHERE repo = ?1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn enqueue_review(
        &self,
        repo: &str,
        kind: &str,
        pr_number: Option<i64>,
        paths_json: Option<&str>,
    ) -> Result<QueueRow> {
        let now = now_rfc3339();
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO queue (repo, kind, pr_number, paths_json, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'waiting', ?5, ?5) RETURNING id",
        )
        .bind(repo)
        .bind(kind)
        .bind(pr_number)
        .bind(paths_json)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, QueueRow>(
            "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
             FROM queue WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn list_queue(&self, repo: Option<&str>) -> Result<Vec<QueueRow>> {
        let rows = match repo {
            Some(r) => {
                sqlx::query_as::<_, QueueRow>(
                    "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
                     FROM queue WHERE repo = ?1 ORDER BY priority DESC, id ASC",
                )
                .bind(r)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as::<_, QueueRow>(
                    "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
                     FROM queue ORDER BY priority DESC, id ASC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    pub async fn list_findings(
        &self,
        repo: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<FindingRow>> {
        let rows = match (repo, status) {
            (Some(r), Some(s)) => {
                sqlx::query_as::<_, FindingRow>(
                    "SELECT id, repo, path, line, commit_sha, severity, category, title, body, fingerprint, status, review_run, issue_number, created_at
                     FROM findings WHERE repo = ?1 AND status = ?2 ORDER BY id DESC",
                )
                .bind(r)
                .bind(s)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(r), None) => {
                sqlx::query_as::<_, FindingRow>(
                    "SELECT id, repo, path, line, commit_sha, severity, category, title, body, fingerprint, status, review_run, issue_number, created_at
                     FROM findings WHERE repo = ?1 ORDER BY id DESC",
                )
                .bind(r)
                .fetch_all(&self.pool)
                .await?
            }
            (None, Some(s)) => {
                sqlx::query_as::<_, FindingRow>(
                    "SELECT id, repo, path, line, commit_sha, severity, category, title, body, fingerprint, status, review_run, issue_number, created_at
                     FROM findings WHERE status = ?1 ORDER BY id DESC",
                )
                .bind(s)
                .fetch_all(&self.pool)
                .await?
            }
            (None, None) => {
                sqlx::query_as::<_, FindingRow>(
                    "SELECT id, repo, path, line, commit_sha, severity, category, title, body, fingerprint, status, review_run, issue_number, created_at
                     FROM findings ORDER BY id DESC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    pub async fn update_finding_status(&self, id: i64, new_status: &str, actor: &str) -> Result<bool> {
        let current: Option<String> =
            sqlx::query_scalar("SELECT status FROM findings WHERE id = ?1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(from_status) = current else {
            return Ok(false);
        };
        let now = now_rfc3339();
        sqlx::query("UPDATE findings SET status = ?1 WHERE id = ?2")
            .bind(new_status)
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO finding_events (finding_id, at, from_status, to_status, actor)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(&now)
        .bind(&from_status)
        .bind(new_status)
        .bind(actor)
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    pub async fn get_settings(&self) -> Result<SettingsRow> {
        let row = sqlx::query_as::<_, SettingsRow>(
            "SELECT id, mode, repo_daily_cap, fleet_daily_cap, quiet_minutes, pooled, updated_at
             FROM settings WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn set_mode(&self, mode: Mode) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query("UPDATE settings SET mode = ?1, updated_at = ?2 WHERE id = 1")
            .bind(mode.as_str())
            .bind(&now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_etag(&self, url: &str) -> Result<Option<String>> {
        let etag = sqlx::query_scalar("SELECT etag FROM poll_etags WHERE url = ?1")
            .bind(url)
            .fetch_optional(&self.pool)
            .await?;
        Ok(etag)
    }

    pub async fn set_etag(&self, url: &str, etag: &str) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO poll_etags (url, etag, last_polled_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(url) DO UPDATE SET etag = excluded.etag, last_polled_at = excluded.last_polled_at",
        )
        .bind(url)
        .bind(etag)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
