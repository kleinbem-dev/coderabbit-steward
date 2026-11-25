use crate::budget::{BudgetLedger, Confidence, SpendRecord, SpendState};
use crate::config::Mode;
use crate::coverage::FileCoverageState;
use crate::findings::{ParsedFinding, compute_fingerprint};
use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, SqlitePool};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

pub fn now_rfc3339() -> String {
    Timestamp::now().to_string()
}

#[derive(Clone)]
pub struct Db {
    pub pool: SqlitePool,
    ledger_lock: Arc<Mutex<()>>,
    coverage_lock: Arc<Mutex<()>>,
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

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ReviewRunRow {
    pub id: i64,
    pub repo: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub kind: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: Option<String>,
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
        Ok(Self {
            pool,
            ledger_lock: Arc::new(Mutex::new(())),
            coverage_lock: Arc::new(Mutex::new(())),
        })
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

    /// Serialise load-modify-save of a repo's budget ledger so concurrent webhook deliveries
    /// cannot clobber each other's throttle deadlines.
    pub async fn update_budget_ledger<F>(
        &self,
        repo: &str,
        now: Timestamp,
        mutate: F,
    ) -> Result<BudgetLedger>
    where
        F: FnOnce(&mut BudgetLedger),
    {
        let _guard = self.ledger_lock.lock().await;
        let mut ledger = self.load_budget_ledger(repo).await?;
        mutate(&mut ledger);
        self.save_budget_ledger(repo, &ledger, now).await?;
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

    /// Atomically enqueue multiple batch rows (e.g. full-repo area batches) in one transaction.
    pub async fn enqueue_review_batches(
        &self,
        repo: &str,
        kind: &str,
        batches_json: &[String],
    ) -> Result<QueueRow> {
        let now = now_rfc3339();
        let mut tx = self.pool.begin().await?;
        let mut first_id: Option<i64> = None;
        for paths_json in batches_json {
            let id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO queue (repo, kind, pr_number, paths_json, state, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, 'waiting', ?4, ?4) RETURNING id",
            )
            .bind(repo)
            .bind(kind)
            .bind(paths_json)
            .bind(&now)
            .fetch_one(&mut *tx)
            .await?;
            if first_id.is_none() {
                first_id = Some(id);
            }
        }
        let first_id = first_id.context("batches_json must be non-empty")?;
        let row = sqlx::query_as::<_, QueueRow>(
            "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
             FROM queue WHERE id = ?1",
        )
        .bind(first_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
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

    pub async fn list_active_queue(&self, repo: &str) -> Result<Vec<QueueRow>> {
        let rows = sqlx::query_as::<_, QueueRow>(
            "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
             FROM queue
             WHERE repo = ?1 AND state IN ('waiting', 'triggered')
             ORDER BY priority DESC, id ASC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn enqueue_pr_if_absent(
        &self,
        repo: &str,
        pr_number: i64,
        not_before: Option<&str>,
    ) -> Result<Option<QueueRow>> {
        let now = now_rfc3339();
        let inserted_id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO queue (repo, kind, pr_number, state, not_before, created_at, updated_at)
             SELECT ?1, 'on-demand', ?2, 'waiting', ?3, ?4, ?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM queue
                 WHERE repo = ?1 AND pr_number = ?2 AND state IN ('waiting', 'triggered')
             )
             RETURNING id",
        )
        .bind(repo)
        .bind(pr_number)
        .bind(not_before)
        .bind(&now)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(id) = inserted_id {
            let row = sqlx::query_as::<_, QueueRow>(
                "SELECT id, repo, kind, pr_number, paths_json, priority, state, not_before, created_at, updated_at
                 FROM queue WHERE id = ?1",
            )
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
            return Ok(Some(row));
        }

        // Return any rate-limited `triggered` row back to `waiting` so the scheduler retries it.
        sqlx::query(
            "UPDATE queue
             SET state = 'waiting',
                 not_before = COALESCE(?1, not_before),
                 updated_at = ?2
             WHERE repo = ?3 AND pr_number = ?4 AND state IN ('waiting', 'triggered')",
        )
        .bind(not_before)
        .bind(&now)
        .bind(repo)
        .bind(pr_number)
        .execute(&self.pool)
        .await?;
        Ok(None)
    }

    pub async fn mark_queue_triggered_if_waiting(&self, id: i64) -> Result<bool> {
        let now = now_rfc3339();
        let res = sqlx::query(
            "UPDATE queue SET state = 'triggered', updated_at = ?1 WHERE id = ?2 AND state = 'waiting'",
        )
        .bind(&now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn update_queue_state(
        &self,
        id: i64,
        state: &str,
        not_before: Option<&str>,
    ) -> Result<()> {
        let now = now_rfc3339();
        if let Some(nb) = not_before {
            sqlx::query("UPDATE queue SET state = ?1, not_before = ?2, updated_at = ?3 WHERE id = ?4")
                .bind(state)
                .bind(nb)
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        } else {
            sqlx::query("UPDATE queue SET state = ?1, updated_at = ?2 WHERE id = ?3")
                .bind(state)
                .bind(&now)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn complete_queue_for_pr(
        &self,
        repo: &str,
        pr_number: i64,
        state: &str,
    ) -> Result<u64> {
        let now = now_rfc3339();
        let res = sqlx::query(
            "UPDATE queue SET state = ?1, updated_at = ?2
             WHERE repo = ?3 AND pr_number = ?4 AND state IN ('waiting', 'triggered')",
        )
        .bind(state)
        .bind(&now)
        .bind(repo)
        .bind(pr_number)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
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

    pub async fn get_scan_cursor(&self, repo: &str) -> Result<Option<String>> {
        let sha = sqlx::query_scalar("SELECT scanned_through FROM scan_cursor WHERE repo = ?1")
            .bind(repo)
            .fetch_optional(&self.pool)
            .await?;
        Ok(sha)
    }

    pub async fn set_scan_cursor(&self, repo: &str, sha: &str) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query(
            "INSERT INTO scan_cursor (repo, scanned_through, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo) DO UPDATE SET scanned_through = excluded.scanned_through, updated_at = excluded.updated_at",
        )
        .bind(repo)
        .bind(sha)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_file_coverage(&self, repo: &str) -> Result<Vec<FileCoverageState>> {
        #[derive(FromRow)]
        struct RawRow {
            path: String,
            reviewed_blob: Option<String>,
            reviewed_commit: Option<String>,
            by: Option<String>,
            unreviewed_lines: i64,
            unreviewed_since: Option<String>,
            excluded: i64,
        }
        let rows = sqlx::query_as::<_, RawRow>(
            "SELECT path, reviewed_blob, reviewed_commit, by, unreviewed_lines, unreviewed_since, excluded
             FROM file_coverage WHERE repo = ?1 ORDER BY path",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| FileCoverageState {
                path: r.path,
                reviewed_blob: r.reviewed_blob,
                reviewed_commit: r.reviewed_commit,
                by: r.by,
                unreviewed_lines: r.unreviewed_lines,
                unreviewed_since: r.unreviewed_since,
                excluded: r.excluded != 0,
            })
            .collect())
    }

    pub async fn upsert_file_coverage(&self, repo: &str, state: &FileCoverageState) -> Result<()> {
        sqlx::query(
            "INSERT INTO file_coverage (repo, path, reviewed_blob, reviewed_commit, by, unreviewed_lines, unreviewed_since, excluded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(repo, path) DO UPDATE SET
               reviewed_blob = excluded.reviewed_blob,
               reviewed_commit = excluded.reviewed_commit,
               by = excluded.by,
               unreviewed_lines = excluded.unreviewed_lines,
               unreviewed_since = excluded.unreviewed_since,
               excluded = excluded.excluded",
        )
        .bind(repo)
        .bind(&state.path)
        .bind(&state.reviewed_blob)
        .bind(&state.reviewed_commit)
        .bind(&state.by)
        .bind(state.unreviewed_lines)
        .bind(&state.unreviewed_since)
        .bind(if state.excluded { 1 } else { 0 })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_file_coverage(&self, repo: &str, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM file_coverage WHERE repo = ?1 AND path = ?2")
            .bind(repo)
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Persist coverage deletions, upserts, and scan cursor advancement atomically in one transaction.
    pub async fn commit_coverage_batch(
        &self,
        repo: &str,
        deleted_paths: &[String],
        upserts: &[FileCoverageState],
        new_cursor: Option<&str>,
    ) -> Result<()> {
        let _guard = self.coverage_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        for path in deleted_paths {
            sqlx::query("DELETE FROM file_coverage WHERE repo = ?1 AND path = ?2")
                .bind(repo)
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        for state in upserts {
            sqlx::query(
                "INSERT INTO file_coverage (repo, path, reviewed_blob, reviewed_commit, by, unreviewed_lines, unreviewed_since, excluded)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(repo, path) DO UPDATE SET
                   reviewed_blob = excluded.reviewed_blob,
                   reviewed_commit = excluded.reviewed_commit,
                   by = excluded.by,
                   unreviewed_lines = excluded.unreviewed_lines,
                   unreviewed_since = excluded.unreviewed_since,
                   excluded = excluded.excluded",
            )
            .bind(repo)
            .bind(&state.path)
            .bind(&state.reviewed_blob)
            .bind(&state.reviewed_commit)
            .bind(&state.by)
            .bind(state.unreviewed_lines)
            .bind(&state.unreviewed_since)
            .bind(if state.excluded { 1 } else { 0 })
            .execute(&mut *tx)
            .await?;
        }
        if let Some(sha) = new_cursor {
            let now = now_rfc3339();
            sqlx::query(
                "INSERT INTO scan_cursor (repo, scanned_through, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo) DO UPDATE SET scanned_through = excluded.scanned_through, updated_at = excluded.updated_at",
            )
            .bind(repo)
            .bind(sha)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Serialised read-modify-write for Revue coverage reports: only touches reported paths and
    /// preserves newer unreviewed changes if the report is for a stale commit/blob.
    pub async fn apply_revue_coverage(
        &self,
        repo: &str,
        commit_sha: &str,
        reported: &[(String, Option<String>)],
    ) -> Result<Vec<FileCoverageState>> {
        let _guard = self.coverage_lock.lock().await;
        let mut tx = self.pool.begin().await?;
        for (path, blob_sha) in reported {
            if crate::coverage::is_excluded_path(path) {
                continue;
            }
            let existing: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
                "SELECT reviewed_blob, reviewed_commit, unreviewed_lines
                 FROM file_coverage WHERE repo = ?1 AND path = ?2",
            )
            .bind(repo)
            .bind(path)
            .fetch_optional(&mut *tx)
            .await?;

            // If an existing row already has a different reviewed_commit and newer unreviewed
            // lines, and the incoming report's blob does not match, avoid clearing newer changes.
            if let Some((ref cur_blob, Some(ref cur_commit), unrev)) = existing
                && cur_commit != commit_sha
                && unrev > 0
                && blob_sha.is_some()
                && cur_blob.as_deref() != blob_sha.as_deref()
            {
                continue;
            }

            sqlx::query(
                "INSERT INTO file_coverage (repo, path, reviewed_blob, reviewed_commit, by, unreviewed_lines, unreviewed_since, excluded)
                 VALUES (?1, ?2, ?3, ?4, 'revue', 0, NULL, 0)
                 ON CONFLICT(repo, path) DO UPDATE SET
                   reviewed_blob = COALESCE(excluded.reviewed_blob, file_coverage.reviewed_blob),
                   reviewed_commit = excluded.reviewed_commit,
                   by = 'revue',
                   unreviewed_lines = 0,
                   unreviewed_since = NULL,
                   excluded = 0",
            )
            .bind(repo)
            .bind(path)
            .bind(blob_sha)
            .bind(commit_sha)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        drop(_guard);
        self.list_file_coverage(repo).await
    }

    /// Insert or update a finding deduplicated by `(repo, fingerprint)`, preserving existing status.
    /// Returns `(finding_id, is_new)`.
    pub async fn upsert_finding(
        &self,
        repo: &str,
        finding: &ParsedFinding,
        commit_sha: Option<&str>,
        review_run: Option<i64>,
    ) -> Result<(i64, bool)> {
        let fp = compute_fingerprint(repo, &finding.path, &finding.title, &finding.code_quote);
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM findings WHERE repo = ?1 AND fingerprint = ?2 LIMIT 1",
        )
        .bind(repo)
        .bind(&fp)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(id) = existing {
            sqlx::query(
                "UPDATE findings SET line = ?1, commit_sha = COALESCE(?2, commit_sha), body = ?3, review_run = COALESCE(?4, review_run)
                 WHERE id = ?5",
            )
            .bind(finding.line)
            .bind(commit_sha)
            .bind(&finding.body)
            .bind(review_run)
            .bind(id)
            .execute(&self.pool)
            .await?;
            return Ok((id, false));
        }

        let now = now_rfc3339();
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO findings (repo, path, line, commit_sha, severity, category, title, body, fingerprint, status, review_run, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'new', ?10, ?11)
             RETURNING id",
        )
        .bind(repo)
        .bind(&finding.path)
        .bind(finding.line)
        .bind(commit_sha)
        .bind(&finding.severity)
        .bind(&finding.category)
        .bind(&finding.title)
        .bind(&finding.body)
        .bind(&fp)
        .bind(review_run)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok((id, true))
    }

    pub async fn get_findings_issue_number(&self, repo: &str) -> Result<Option<i64>> {
        let num = sqlx::query_scalar(
            "SELECT issue_number FROM findings WHERE repo = ?1 AND issue_number IS NOT NULL ORDER BY id DESC LIMIT 1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        Ok(num)
    }

    pub async fn set_findings_issue_number(&self, repo: &str, issue_number: i64) -> Result<()> {
        sqlx::query("UPDATE findings SET issue_number = ?1 WHERE repo = ?2")
            .bind(issue_number)
            .bind(repo)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn record_review_run(
        &self,
        repo: &str,
        pr_number: i64,
        head_sha: &str,
        kind: &str,
        metadata: Option<&str>,
    ) -> Result<i64> {
        let now = now_rfc3339();
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO review_runs (repo, pr_number, head_sha, kind, started_at, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
        )
        .bind(repo)
        .bind(pr_number)
        .bind(head_sha)
        .bind(kind)
        .bind(&now)
        .bind(metadata)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn finish_review_run(&self, id: i64, outcome: &str) -> Result<()> {
        let now = now_rfc3339();
        sqlx::query("UPDATE review_runs SET finished_at = ?1, outcome = ?2 WHERE id = ?3")
            .bind(&now)
            .bind(outcome)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_open_proactive_runs(&self, repo: &str) -> Result<Vec<ReviewRunRow>> {
        let rows = sqlx::query_as::<_, ReviewRunRow>(
            "SELECT id, repo, pr_number, head_sha, kind, started_at, finished_at, outcome
             FROM review_runs
             WHERE repo = ?1 AND kind = 'proactive' AND finished_at IS NULL
             ORDER BY id ASC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn count_proactive_today(&self, repo: Option<&str>, now: Timestamp) -> Result<u32> {
        let cutoff = (now - SignedDuration::from_hours(24)).to_string();
        let count: i64 = match repo {
            Some(r) => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM review_runs
                     WHERE repo = ?1 AND kind = 'proactive' AND started_at >= ?2",
                )
                .bind(r)
                .bind(&cutoff)
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM review_runs
                     WHERE kind = 'proactive' AND started_at >= ?1",
                )
                .bind(&cutoff)
                .fetch_one(&self.pool)
                .await?
            }
        };
        Ok(u32::try_from(count.max(0)).unwrap_or(0))
    }

    pub async fn quiet_minutes_for_repo(&self, repo: &str, now: Timestamp) -> Result<u32> {
        let latest_obs: Option<String> =
            sqlx::query_scalar("SELECT MAX(seen_at) FROM observations WHERE repo = ?1")
                .bind(repo)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let latest_spend: Option<String> =
            sqlx::query_scalar("SELECT MAX(at) FROM spends WHERE repo = ?1")
                .bind(repo)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let latest = [latest_obs, latest_spend]
            .into_iter()
            .flatten()
            .filter_map(|s| s.parse::<Timestamp>().ok())
            .max();
        Ok(match latest {
            Some(ts) => {
                let mins = now.duration_since(ts).as_secs().max(0) / 60;
                u32::try_from(mins).unwrap_or(u32::MAX)
            }
            None => 9999,
        })
    }
}
