use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use coderabbit_steward::actuator::github::GitHubActuator;
use coderabbit_steward::actuator::ntfy::NtfyClient;
use coderabbit_steward::actuator::revue::RevueClient;
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::budget::BudgetSnapshot;
use coderabbit_steward::config::{Config, Mode, resolve_web_dir};
use coderabbit_steward::coverage::{CommitVerdict, apply_commit_changes};
use coderabbit_steward::db::Db;
use coderabbit_steward::findings::render_findings_issue_body;
use coderabbit_steward::ingest::poll::GitHubPoller;
use coderabbit_steward::parse::check_run::CheckRunOutcome;
use coderabbit_steward::scheduler::{QueueCandidate, RepoScheduleInput, ScheduledAction, decide};
use coderabbit_steward::synthetic::{batch_from_paths, select_proactive_batch};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Parser)]
#[command(name = "cr-steward", version, about = "CodeRabbit OSS budget and review steward")]
struct Cli {
    /// Base URL for thin client subcommands
    #[arg(long, env = "CR_STEWARD_URL", default_value = "http://127.0.0.1:8484", global = true)]
    url: String,

    /// Optional API bearer token
    #[arg(long, env = "CR_STEWARD_API_TOKEN", global = true)]
    api_token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Args)]
struct DaemonArgs {
    #[arg(long, env = "CR_STEWARD_LISTEN", default_value = "127.0.0.1:8484")]
    listen: String,
    #[arg(long, env = "DATABASE_URL", default_value = "sqlite://coderabbit-steward.db?mode=rwc")]
    database_url: String,
    #[arg(long, env = "CR_STEWARD_WEB_DIR")]
    web_dir: Option<PathBuf>,
    #[arg(long, env = "CR_STEWARD_MODE", default_value = "observe")]
    mode: String,
    #[arg(long, env = "CR_STEWARD_POLL_INTERVAL_SECS", default_value_t = 60)]
    poll_interval_secs: u64,
    #[arg(long, env = "CR_STEWARD_OWNERS", value_delimiter = ',', default_value = "kleinbem,kleinbem-dev")]
    owners: Vec<String>,
    #[arg(long, env = "GITHUB_APP_ID")]
    github_app_id: Option<u64>,
    #[arg(long, env = "GITHUB_APP_PRIVATE_KEY_FILE")]
    github_private_key_file: Option<PathBuf>,
    #[arg(long, env = "GITHUB_APP_PRIVATE_KEY")]
    github_private_key_pem: Option<String>,
    #[arg(long, env = "GITHUB_TOKEN")]
    github_token: Option<String>,
    #[arg(long, env = "CR_STEWARD_USER_TOKEN")]
    github_user_token: Option<String>,
    #[arg(long, env = "GITHUB_WEBHOOK_SECRET")]
    github_webhook_secret: Option<String>,
    #[arg(long, env = "CR_STEWARD_NTFY_URL")]
    ntfy_url: Option<String>,
    #[arg(long, env = "CR_STEWARD_NTFY_TOKEN")]
    ntfy_token: Option<String>,
    #[arg(long, env = "CR_STEWARD_REVUE_URL")]
    revue_url: Option<String>,
    #[arg(long, env = "CR_STEWARD_REVUE_TOKEN")]
    revue_token: Option<String>,
    #[arg(long, env = "CR_STEWARD_TRUST_AUTHENTIK_HEADER", default_value_t = false)]
    trust_authentik_header: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP server, poller, and scheduler daemon
    Daemon(Box<DaemonArgs>),
    /// Check if a repository has free CodeRabbit review budget (exit 0 = free)
    Free {
        /// Repository in owner/name format
        repo: String,
    },
    /// Show the review queue
    Queue {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Request a review (on-demand PR, proactive paths, or full repo)
    Request {
        repo: String,
        pr: Option<i64>,
        #[arg(long, value_delimiter = ',')]
        paths: Option<Vec<String>>,
        #[arg(long)]
        full: bool,
    },
    /// Show coverage for a repository
    Coverage {
        repo: String,
    },
    /// List findings
    Findings {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Commands::Daemon(args) => {
            let DaemonArgs {
                listen,
                database_url,
                web_dir,
                mode,
                poll_interval_secs,
                owners,
                github_app_id,
                github_private_key_file,
                github_private_key_pem,
                github_token,
                github_user_token,
                github_webhook_secret,
                ntfy_url,
                ntfy_token,
                revue_url,
                revue_token,
                trust_authentik_header,
            } = *args;
            let parsed_mode = Mode::parse(&mode)?;
            let config = Config {
                listen: listen.clone(),
                database_url: database_url.clone(),
                web_dir: web_dir.clone(),
                owners,
                mode: parsed_mode,
                poll_interval_secs,
                github_app_id,
                github_private_key_path: github_private_key_file,
                github_private_key_pem,
                github_token,
                github_user_token,
                github_webhook_secret: github_webhook_secret.clone(),
                api_token: cli.api_token.clone(),
                ntfy_url: ntfy_url.clone(),
                ntfy_token: ntfy_token.clone(),
                revue_url: revue_url.clone(),
                revue_token: revue_token.clone(),
                ..Config::default()
            };

            let db = Db::connect(&database_url).await?;
            db.set_mode(parsed_mode).await?;
            let resolved_web = resolve_web_dir(config.web_dir.as_ref());
            let ntfy = ntfy_url
                .filter(|u| !u.trim().is_empty())
                .map(|url| NtfyClient::new(url, ntfy_token))
                .transpose()?;
            let revue = revue_url
                .filter(|u| !u.trim().is_empty())
                .map(|url| RevueClient::new(url, revue_token))
                .transpose()?;
            info!(
                listen = %config.listen,
                mode = %config.mode.as_str(),
                web_dir = ?resolved_web,
                webhook_enabled = github_webhook_secret.as_deref().is_some_and(|s| !s.is_empty()),
                ntfy_enabled = ntfy.is_some(),
                revue_enabled = revue.is_some(),
                "starting cr-steward daemon"
            );

            let mut poller = GitHubPoller::new(config.clone(), db.clone())?;
            let interval = Duration::from_secs(config.poll_interval_secs.max(10));
            tokio::spawn(async move {
                loop {
                    if let Err(err) = poller.poll_once().await {
                        error!(error = %err, "poll loop error");
                    }
                    tokio::time::sleep(interval).await;
                }
            });

            let mut actuator = GitHubActuator::new(config.clone())?;
            let sched_db = db.clone();
            let sched_ntfy = ntfy.clone();
            let sched_revue = revue.clone();
            tokio::spawn(async move {
                let tick = Duration::from_secs(60);
                let mut revue_offered: HashSet<i64> = HashSet::new();
                loop {
                    tokio::time::sleep(tick).await;
                    if let Err(err) = run_scheduler_tick(
                        &sched_db,
                        &mut actuator,
                        sched_ntfy.as_ref(),
                        sched_revue.as_ref(),
                        &mut revue_offered,
                    )
                    .await
                    {
                        error!(error = %err, "scheduler tick error");
                    }
                }
            });

            let state = AppState {
                db,
                api_token: cli.api_token,
                trust_authentik_header,
                webhook_secret: github_webhook_secret,
                ntfy,
                revue,
            };
            let router = build_router(state, resolved_web);
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("failed to bind {listen}"))?;
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            Ok(ExitCode::SUCCESS)
        }
        Commands::Free { repo } => {
            let client = reqwest::Client::new();
            let endpoint = format!("{}/api/v1/budget/{repo}", cli.url.trim_end_matches('/'));
            let mut req = client.get(&endpoint);
            if let Some(tok) = &cli.api_token {
                req = req.bearer_auth(tok);
            }
            let snap: BudgetSnapshot = req.send().await?.error_for_status()?.json().await?;
            if snap.free {
                println!(
                    "free ({}/{} remaining, confidence: {})",
                    snap.remaining,
                    snap.limit,
                    snap.confidence.as_str()
                );
                Ok(ExitCode::SUCCESS)
            } else {
                let tz = TimeZone::get("Europe/Dublin").unwrap_or(TimeZone::UTC);
                let next = snap
                    .next_free_at
                    .map(|t| t.to_zoned(tz).strftime("%Y-%m-%d %H:%M %Z").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "rate-limited (next free: {next}, confidence: {})",
                    snap.confidence.as_str()
                );
                Ok(ExitCode::from(1))
            }
        }
        Commands::Queue { repo } => {
            let client = reqwest::Client::new();
            let mut endpoint = format!("{}/api/v1/queue", cli.url.trim_end_matches('/'));
            if let Some(r) = repo {
                endpoint.push_str(&format!("?repo={r}"));
            }
            let mut req = client.get(&endpoint);
            if let Some(tok) = &cli.api_token {
                req = req.bearer_auth(tok);
            }
            let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::Request {
            repo,
            pr,
            paths,
            full,
        } => {
            let client = reqwest::Client::new();
            let endpoint = format!("{}/api/v1/reviews", cli.url.trim_end_matches('/'));
            let mut req = client.post(&endpoint).json(&serde_json::json!({
                "repo": repo,
                "pr": pr,
                "paths": paths,
                "full": full,
            }));
            if let Some(tok) = &cli.api_token {
                req = req.bearer_auth(tok);
            }
            let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::Coverage { repo } => {
            let client = reqwest::Client::new();
            let endpoint = format!("{}/api/v1/coverage/{repo}", cli.url.trim_end_matches('/'));
            let mut req = client.get(&endpoint);
            if let Some(tok) = &cli.api_token {
                req = req.bearer_auth(tok);
            }
            let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(ExitCode::SUCCESS)
        }
        Commands::Findings { repo, status, json } => {
            let client = reqwest::Client::new();
            let mut endpoint = format!("{}/api/v1/findings", cli.url.trim_end_matches('/'));
            let mut params = Vec::new();
            if let Some(r) = repo {
                params.push(format!("repo={r}"));
            }
            if let Some(s) = status {
                params.push(format!("status={s}"));
            }
            if !params.is_empty() {
                endpoint.push('?');
                endpoint.push_str(&params.join("&"));
            }
            let mut req = client.get(&endpoint);
            if let Some(tok) = &cli.api_token {
                req = req.bearer_auth(tok);
            }
            let body: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
            if json {
                println!("{body}");
            } else {
                println!("{}", serde_json::to_string_pretty(&body)?);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn run_scheduler_tick(
    db: &Db,
    actuator: &mut GitHubActuator,
    ntfy: Option<&NtfyClient>,
    revue: Option<&RevueClient>,
    revue_offered: &mut HashSet<i64>,
) -> Result<()> {
    let settings = db.get_settings().await?;
    let mode = Mode::parse(&settings.mode).unwrap_or(Mode::Observe);
    let repos = db.list_repos().await?;
    let now = Timestamp::now();
    let timeout_cutoff = now - SignedDuration::from_mins(15);
    let mut proactive_today_fleet = db.count_proactive_today(None, now).await?;

    for repo in repos {
        let full = format!("{}/{}", repo.owner, repo.name);

        // Reconcile any open proactive synthetic PRs first (collect findings, advance coverage, close PR).
        let open_proactive = db.list_open_proactive_runs(&full).await?;
        let proactive_timeout_cutoff = now - SignedDuration::from_mins(60);
        for run in &open_proactive {
            let pr_u64 = u64::try_from(run.pr_number).unwrap_or(0);
            if pr_u64 == 0 {
                continue;
            }
            let owned_branches: Vec<String> = run
                .outcome
                .as_deref()
                .and_then(|s| s.strip_prefix("branches:"))
                .map(|s| s.split('|').map(ToString::to_string).collect())
                .unwrap_or_default();

            match actuator.pr_coderabbit_outcome(&full, &run.head_sha).await {
                Ok(Some(CheckRunOutcome::Reviewed { .. })) => {
                    let parsed = match actuator.fetch_pr_findings(&full, pr_u64).await {
                        Ok(f) => f,
                        Err(err) => {
                            warn!(repo = %full, pr = pr_u64, error = %err, "failed to fetch proactive PR findings; will retry");
                            continue;
                        }
                    };
                    let mut new_critical = 0usize;
                    let mut upsert_failed = false;
                    for f in &parsed {
                        match db
                            .upsert_finding(&full, f, Some(&run.head_sha), Some(run.id))
                            .await
                        {
                            Ok((_, is_new)) => {
                                if is_new && f.severity == "critical" {
                                    new_critical += 1;
                                }
                            }
                            Err(err) => {
                                warn!(repo = %full, pr = pr_u64, error = %err, "failed to upsert finding; keeping run open for retry");
                                upsert_failed = true;
                            }
                        }
                    }
                    if upsert_failed {
                        continue;
                    }

                    // Advance coverage for every file reviewed in this proactive batch before finishing.
                    let reviewed_files = match actuator.fetch_pr_files(&full, pr_u64).await {
                        Ok(files) => files,
                        Err(err) => {
                            warn!(repo = %full, pr = pr_u64, error = %err, "failed to fetch proactive PR files; will retry");
                            continue;
                        }
                    };
                    let mut cov_map: HashMap<_, _> = db
                        .list_file_coverage(&full)
                        .await?
                        .into_iter()
                        .map(|f| (f.path.clone(), f))
                        .collect();
                    apply_commit_changes(
                        &mut cov_map,
                        &reviewed_files,
                        &CommitVerdict::Reviewed {
                            by: "proactive".to_string(),
                            commit_sha: run.head_sha.clone(),
                        },
                    );
                    let upserts: Vec<_> = cov_map.into_values().collect();
                    db.commit_coverage_batch(&full, &[], &upserts, None).await?;

                    if new_critical > 0
                        && let Some(client) = ntfy
                    {
                        client.send(
                            &format!("cr-steward: {new_critical} CRITICAL finding(s) in {full}"),
                            &format!("Proactive review PR #{pr_u64} found {new_critical} new critical finding(s)."),
                            "urgent",
                        );
                    }
                    let all_findings = db.list_findings(Some(&full), None).await.unwrap_or_default();
                    let open_findings: Vec<_> = all_findings
                        .into_iter()
                        .filter(|f| matches!(f.status.as_str(), "new" | "real" | "needs-martin"))
                        .collect();
                    let issue_body = render_findings_issue_body(&full, &open_findings);
                    let existing_issue = db.get_findings_issue_number(&full).await.unwrap_or(None);
                    if let Ok(Some(num)) = actuator
                        .sync_findings_issue(&full, existing_issue, &issue_body, !open_findings.is_empty())
                        .await
                    {
                        let _ = db.set_findings_issue_number(&full, num).await;
                    }
                    let comment = format!(
                        "CodeRabbit finished reviewing this proactive batch ({} finding(s) recorded). Closing synthetic PR and cleaning up `review/**` branches.",
                        parsed.len()
                    );
                    if let Err(err) = actuator
                        .close_proactive_pr_and_cleanup(&full, pr_u64, &comment, &owned_branches)
                        .await
                    {
                        warn!(repo = %full, pr = pr_u64, error = %err, "failed to close proactive PR");
                    }
                    db.finish_review_run(run.id, "reviewed").await?;
                    info!(repo = %full, pr = pr_u64, findings = parsed.len(), "completed proactive review run");
                }
                Ok(Some(CheckRunOutcome::RateLimited { .. })) => {
                    let _ = actuator
                        .close_proactive_pr_and_cleanup(
                            &full,
                            pr_u64,
                            "Rate-limited by CodeRabbit; closing synthetic PR without advancing coverage.",
                            &owned_branches,
                        )
                        .await;
                    db.finish_review_run(run.id, "rate_limited").await?;
                }
                _ => {
                    let closed = actuator.is_pr_closed(&full, pr_u64).await.unwrap_or(false);
                    let timed_out = run
                        .started_at
                        .parse::<Timestamp>()
                        .is_ok_and(|ts| ts < proactive_timeout_cutoff);
                    if closed || timed_out {
                        let outcome = if closed { "closed" } else { "timeout" };
                        if !closed {
                            let _ = actuator
                                .close_proactive_pr_and_cleanup(
                                    &full,
                                    pr_u64,
                                    "Proactive review timed out without a CodeRabbit outcome; closing synthetic PR.",
                                    &owned_branches,
                                )
                                .await;
                        }
                        db.finish_review_run(run.id, outcome).await?;
                        warn!(repo = %full, pr = pr_u64, outcome, "closed stale proactive review run");
                    }
                }
            }
        }

        let ledger = db.load_budget_ledger(&full).await?;
        let snap = ledger.snapshot(now);
        let active = db.list_active_queue(&full).await?;

        let triggered = active.iter().find(|q| q.state == "triggered");
        let has_pending_trigger = triggered.is_some() || !db.list_open_proactive_runs(&full).await?.is_empty();
        let pending_timed_out = triggered.is_some_and(|q| {
            q.updated_at
                .parse::<Timestamp>()
                .is_ok_and(|ts| ts < timeout_cutoff)
        });

        let on_demand_queue: Vec<QueueCandidate> = active
            .iter()
            .filter(|q| q.state == "waiting" && q.kind == "on-demand")
            .filter(|q| q.pr_number.is_some())
            .filter(|q| {
                q.not_before
                    .as_deref()
                    .and_then(|s| s.parse::<Timestamp>().ok())
                    .is_none_or(|nb| nb <= now)
            })
            .map(|q| QueueCandidate {
                id: q.id,
                pr_number: q.pr_number.and_then(|n| u64::try_from(n).ok()),
                risky_paths: q.priority > 0,
                created_at: q.created_at.parse::<Timestamp>().unwrap_or(now),
            })
            .collect();

        let revue_wait_cutoff = now - SignedDuration::from_mins(90);
        if let Some(revue_client) = revue {
            for q in &active {
                if q.state == "waiting"
                    && q.kind == "on-demand"
                    && let Some(pr) = q.pr_number.and_then(|n| u64::try_from(n).ok())
                    && q.created_at
                        .parse::<Timestamp>()
                        .is_ok_and(|ts| ts < revue_wait_cutoff)
                    && !revue_offered.contains(&q.id)
                    && revue_client.trigger_advisory_review(&full, pr, "queue-wait-90m")
                {
                    revue_offered.insert(q.id);
                }
            }
        }

        let cov_files = db.list_file_coverage(&full).await?;
        let queued_choice = active
            .iter()
            .filter(|q| q.state == "waiting" && (q.kind == "full" || q.kind == "proactive"))
            .find_map(|q| {
                let paths: Vec<String> = q
                    .paths_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                batch_from_paths(&paths, &cov_files, q.kind == "full").map(|b| (Some(q.id), b))
            });
        let proactive_choice = queued_choice.or_else(|| {
            select_proactive_batch(
                &cov_files,
                now,
                usize::try_from(repo.file_cap.max(1)).unwrap_or(50),
                2000,
            )
            .map(|b| (None, b))
        });
        let quiet_for_minutes = db.quiet_minutes_for_repo(&full, now).await?;
        let proactive_today_repo = db.count_proactive_today(Some(&full), now).await?;

        let input = RepoScheduleInput {
            repo: full.clone(),
            mode,
            budget: snap,
            has_pending_trigger,
            pending_timed_out,
            on_demand_queue,
            quiet_for_minutes,
            required_quiet_minutes: u32::try_from(settings.quiet_minutes.max(0)).unwrap_or(45),
            predicted_demand: 0.0,
            open_unlabelled_risky_pr: false,
            proactive_today_repo,
            repo_daily_cap: u32::try_from(settings.repo_daily_cap.max(0)).unwrap_or(6),
            proactive_today_fleet,
            fleet_daily_cap: u32::try_from(settings.fleet_daily_cap.max(0)).unwrap_or(40),
            has_proactive_candidate: proactive_choice.is_some(),
        };

        match decide(&input) {
            ScheduledAction::TimeoutPending => {
                if let Some(q) = triggered {
                    warn!(repo = %full, queue_id = q.id, pr = ?q.pr_number, "pending trigger timed out after 15m");
                    db.update_queue_state(q.id, "timeout", None).await?;
                    if let Some(client) = ntfy {
                        client.send(
                            &format!("cr-steward: stuck trigger in {full}"),
                            &format!(
                                "Queue item {} (PR {:?}) stayed in triggered state for >15m without a CodeRabbit response.",
                                q.id, q.pr_number
                            ),
                            "high",
                        );
                    }
                }
            }
            ScheduledAction::TriggerOnDemand {
                queue_id,
                pr_number: Some(pr),
            } => {
                info!(repo = %full, queue_id, pr, "triggering queued on-demand CodeRabbit review");
                match actuator.retrigger_cr_review_label(&full, pr).await {
                    Ok(()) => {
                        if db.mark_queue_triggered_if_waiting(queue_id).await? {
                            match actuator.get_pr_head_sha(&full, pr).await {
                                Ok(Some(sha)) => {
                                    if let Err(err) = actuator
                                        .post_steward_check_run(
                                            &full,
                                            &sha,
                                            "Re-triggered CodeRabbit review",
                                            "Budget became free; re-applied `cr-review` label.",
                                            "completed",
                                            Some("neutral"),
                                        )
                                        .await
                                    {
                                        warn!(repo = %full, pr, error = %err, "failed to post cr-steward check run");
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    warn!(repo = %full, pr, error = %err, "failed to fetch PR head sha");
                                }
                            }
                        }
                    }
                    Err(err) => {
                        error!(repo = %full, queue_id, pr, error = %err, "failed to re-trigger cr-review label");
                        if let Some(client) = ntfy {
                            client.send(
                                &format!("cr-steward: re-trigger failed in {full}#{pr}"),
                                &format!("{err:#}"),
                                "high",
                            );
                        }
                    }
                }
            }
            ScheduledAction::TriggerOnDemand {
                queue_id,
                pr_number: None,
            } => {
                warn!(repo = %full, queue_id, "on-demand queue item missing pr_number");
            }
            ScheduledAction::WaitPending => {
                debug!(repo = %full, "waiting on pending trigger");
            }
            ScheduledAction::Idle { reason } => {
                debug!(repo = %full, %reason, "scheduler idle");
            }
            ScheduledAction::TriggerProactive => {
                if let Some((queued_id, batch)) = proactive_choice {
                    info!(
                        repo = %full,
                        area = %batch.area,
                        files = batch.files.len(),
                        lines = batch.total_lines,
                        "opening proactive synthetic review PR"
                    );
                    match actuator
                        .open_proactive_pr(&full, &repo.default_branch, &batch)
                        .await
                    {
                        Ok(opened) => {
                            let branch_meta =
                                format!("branches:{}|{}", opened.base_branch, opened.head_branch);
                            db.record_review_run(
                                &full,
                                i64::try_from(opened.pr_number).unwrap_or(0),
                                &opened.head_sha,
                                "proactive",
                                Some(&branch_meta),
                            )
                            .await?;
                            if let Some(qid) = queued_id {
                                let _ = db.update_queue_state(qid, "done", None).await;
                            }
                            proactive_today_fleet += 1;
                            info!(
                                repo = %full,
                                pr = opened.pr_number,
                                base = %opened.base_branch,
                                head = %opened.head_branch,
                                "proactive synthetic PR opened"
                            );
                        }
                        Err(err) => {
                            error!(repo = %full, area = %batch.area, error = %err, "failed to open proactive PR");
                            if let Some(client) = ntfy {
                                client.send(
                                    &format!("cr-steward: proactive PR failed in {full}"),
                                    &format!("Area {}: {err:#}", batch.area),
                                    "default",
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
