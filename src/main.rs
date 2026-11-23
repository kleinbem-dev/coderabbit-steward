use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::budget::BudgetSnapshot;
use coderabbit_steward::config::{Config, Mode, resolve_web_dir};
use coderabbit_steward::db::Db;
use coderabbit_steward::ingest::poll::GitHubPoller;
use jiff::tz::TimeZone;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{error, info};

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

#[derive(Subcommand)]
enum Commands {
    /// Run the HTTP server, poller, and scheduler daemon
    Daemon {
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
        #[arg(long, env = "CR_STEWARD_TRUST_AUTHENTIK_HEADER", default_value_t = false)]
        trust_authentik_header: bool,
    },
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
        Commands::Daemon {
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
            trust_authentik_header,
        } => {
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
                api_token: cli.api_token.clone(),
                ..Config::default()
            };

            let db = Db::connect(&database_url).await?;
            db.set_mode(parsed_mode).await?;
            let resolved_web = resolve_web_dir(config.web_dir.as_ref());
            info!(
                listen = %config.listen,
                mode = %config.mode.as_str(),
                web_dir = ?resolved_web,
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

            let state = AppState {
                db,
                api_token: cli.api_token,
                trust_authentik_header,
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
