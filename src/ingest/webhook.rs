use crate::actuator::ntfy::NtfyClient;
use crate::db::Db;
use crate::parse::check_run::{CheckRunOutcome, parse_check_run};
use crate::parse::comment::{BotMessage, parse_issue_comment};
use crate::parse::review::parse_pr_review;
use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::info;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize)]
pub struct WebhookOutcome {
    pub ignored: bool,
    pub message: String,
}

/// Verify GitHub's `X-Hub-Signature-256` header (`sha256=<hex>`) in constant time.
pub fn verify_signature(secret: &str, signature_header: &str, body: &[u8]) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(provided) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    provided.len() == expected.len() && bool::from(provided.ct_eq(expected.as_slice()))
}

/// Process a verified GitHub webhook event.
pub async fn handle_webhook_event(
    db: &Db,
    ntfy: Option<&NtfyClient>,
    event: &str,
    payload: &[u8],
) -> Result<WebhookOutcome> {
    if event == "ping" {
        return Ok(WebhookOutcome {
            ignored: false,
            message: "pong".to_string(),
        });
    }

    let root: Value = serde_json::from_slice(payload).context("invalid webhook JSON")?;
    let Some(repo) = root
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
    else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing repository.full_name".to_string(),
        });
    };

    match event {
        "check_run" => handle_check_run(db, ntfy, repo, &root).await,
        "issue_comment" => handle_issue_comment(db, ntfy, repo, &root).await,
        "pull_request_review" => handle_pr_review(db, ntfy, repo, &root).await,
        "pull_request" => handle_pull_request(db, repo, &root).await,
        other => Ok(WebhookOutcome {
            ignored: true,
            message: format!("unhandled event: {other}"),
        }),
    }
}

async fn handle_check_run(
    db: &Db,
    ntfy: Option<&NtfyClient>,
    repo: &str,
    root: &Value,
) -> Result<WebhookOutcome> {
    let action = root.get("action").and_then(Value::as_str).unwrap_or("");
    if !action.is_empty() && action != "created" && action != "completed" {
        return Ok(WebhookOutcome {
            ignored: true,
            message: format!("ignored check_run action: {action}"),
        });
    }

    let Some(cr) = root.get("check_run") else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing check_run object".to_string(),
        });
    };
    let cr_json = serde_json::to_string(cr)?;
    let Some(outcome) = parse_check_run(&cr_json) else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "non-coderabbit check_run".to_string(),
        });
    };

    let github_id = cr.get("id").and_then(Value::as_i64).unwrap_or(0);
    let pr_number = cr
        .get("pull_requests")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|p| p.get("number"))
        .and_then(Value::as_i64);

    let outcome_json = serde_json::to_string(&outcome)?;
    db.record_observation(
        repo,
        "check_run",
        github_id,
        pr_number,
        &cr_json,
        Some(&outcome_json),
    )
    .await?;

    let is_completed = action.is_empty() || action == "completed";
    let now = Timestamp::now();
    let ledger = db
        .update_budget_ledger(repo, now, |ledger| {
            if is_completed || matches!(outcome, CheckRunOutcome::InProgress { .. }) {
                ledger.apply_check_run(
                    &outcome,
                    pr_number.and_then(|n| u64::try_from(n).ok()),
                    u64::try_from(github_id).ok(),
                    "webhook",
                    "pr-webhook",
                );
            }
        })
        .await?;

    if is_completed {
        match &outcome {
            CheckRunOutcome::RateLimited { .. } => {
                if let Some(pr) = pr_number {
                    let next_free = ledger.snapshot(now).next_free_at.map(|t| t.to_string());
                    if db
                        .enqueue_pr_if_absent(repo, pr, next_free.as_deref())
                        .await?
                        .is_some()
                    {
                        info!(repo, pr, "auto-enqueued rate-limited PR from check_run");
                    }
                }
            }
            CheckRunOutcome::Reviewed { .. } => {
                if let Some(pr) = pr_number {
                    let completed = db.complete_queue_for_pr(repo, pr, "done").await?;
                    if completed > 0 {
                        info!(repo, pr, "completed queued PR after CodeRabbit review");
                    }
                }
            }
            CheckRunOutcome::Unrecognised {
                status,
                conclusion,
                title,
            } => {
                if let Some(client) = ntfy {
                    client.send(
                        &format!("cr-steward: unrecognised check_run in {repo}"),
                        &format!("status={status}, conclusion={conclusion:?}, title={title:?}"),
                        "default",
                    );
                }
            }
            _ => {}
        }
    }

    Ok(WebhookOutcome {
        ignored: false,
        message: "check_run processed".to_string(),
    })
}

async fn handle_issue_comment(
    db: &Db,
    ntfy: Option<&NtfyClient>,
    repo: &str,
    root: &Value,
) -> Result<WebhookOutcome> {
    let action = root.get("action").and_then(Value::as_str).unwrap_or("");
    if !action.is_empty() && action != "created" && action != "edited" {
        return Ok(WebhookOutcome {
            ignored: true,
            message: format!("ignored issue_comment action: {action}"),
        });
    }

    let Some(comment) = root.get("comment") else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing comment object".to_string(),
        });
    };
    let comment_json = serde_json::to_string(comment)?;
    let Some(msg) = parse_issue_comment(&comment_json) else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "non-coderabbit issue_comment".to_string(),
        });
    };

    let github_id = comment.get("id").and_then(Value::as_i64).unwrap_or(0);
    let is_pr = root
        .get("issue")
        .and_then(|i| i.get("pull_request"))
        .is_some();
    let pr_number = if is_pr {
        root.get("issue")
            .and_then(|i| i.get("number"))
            .and_then(Value::as_i64)
    } else {
        None
    };

    let msg_json = serde_json::to_string(&msg)?;
    db.record_observation(
        repo,
        "issue_comment",
        github_id,
        pr_number,
        &comment_json,
        Some(&msg_json),
    )
    .await?;

    let now = Timestamp::now();
    let ledger = db
        .update_budget_ledger(repo, now, |ledger| {
            ledger.apply_bot_message(&msg);
        })
        .await?;

    match &msg {
        BotMessage::RateLimited { .. } => {
            if let Some(pr) = pr_number {
                let next_free = ledger.snapshot(now).next_free_at.map(|t| t.to_string());
                if db
                    .enqueue_pr_if_absent(repo, pr, next_free.as_deref())
                    .await?
                    .is_some()
                {
                    info!(repo, pr, "auto-enqueued rate-limited PR from issue_comment");
                }
            }
        }
        BotMessage::Unrecognised { .. } => {
            if let Some(client) = ntfy {
                let snippet: String = comment
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(240)
                    .collect();
                client.send(
                    &format!("cr-steward: unrecognised CodeRabbit comment in {repo}"),
                    &format!("PR #{pr_number:?}: {snippet}"),
                    "default",
                );
            }
        }
        _ => {}
    }

    Ok(WebhookOutcome {
        ignored: false,
        message: "issue_comment processed".to_string(),
    })
}

async fn handle_pr_review(
    db: &Db,
    ntfy: Option<&NtfyClient>,
    repo: &str,
    root: &Value,
) -> Result<WebhookOutcome> {
    let Some(review) = root.get("review") else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing review object".to_string(),
        });
    };
    let review_json = serde_json::to_string(review)?;
    let Some(msg) = parse_pr_review(&review_json) else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "non-coderabbit pull_request_review".to_string(),
        });
    };

    let github_id = review.get("id").and_then(Value::as_i64).unwrap_or(0);
    let pr_number = root
        .get("pull_request")
        .and_then(|p| p.get("number"))
        .and_then(Value::as_i64);

    let msg_json = serde_json::to_string(&msg)?;
    db.record_observation(
        repo,
        "pull_request_review",
        github_id,
        pr_number,
        &review_json,
        Some(&msg_json),
    )
    .await?;

    let now = Timestamp::now();
    db.update_budget_ledger(repo, now, |ledger| {
        ledger.apply_bot_message(&msg);
    })
    .await?;

    match &msg {
        BotMessage::ReviewSummary { .. } | BotMessage::ReviewFinished { .. } => {
            if let Some(pr) = pr_number {
                let completed = db.complete_queue_for_pr(repo, pr, "done").await?;
                if completed > 0 {
                    info!(repo, pr, "completed queued PR from pull_request_review");
                }
            }
        }
        BotMessage::Unrecognised { .. } => {
            if let Some(client) = ntfy {
                client.send(
                    &format!("cr-steward: unrecognised CodeRabbit review in {repo}"),
                    &format!("PR #{pr_number:?}"),
                    "default",
                );
            }
        }
        _ => {}
    }

    Ok(WebhookOutcome {
        ignored: false,
        message: "pull_request_review processed".to_string(),
    })
}

async fn handle_pull_request(
    db: &Db,
    repo: &str,
    root: &Value,
) -> Result<WebhookOutcome> {
    let action = root.get("action").and_then(Value::as_str).unwrap_or("");
    if action != "closed" {
        return Ok(WebhookOutcome {
            ignored: true,
            message: format!("ignored pull_request action: {action}"),
        });
    }

    let Some(pr) = root.get("pull_request") else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing pull_request object".to_string(),
        });
    };
    let Some(pr_number) = pr.get("number").and_then(Value::as_i64) else {
        return Ok(WebhookOutcome {
            ignored: true,
            message: "missing pull_request.number".to_string(),
        });
    };
    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let target_state = if merged { "merged-unreviewed" } else { "done" };
    let updated = db.complete_queue_for_pr(repo, pr_number, target_state).await?;
    if updated > 0 {
        info!(repo, pr_number, target_state, "updated queued PR on close");
    }

    Ok(WebhookOutcome {
        ignored: false,
        message: format!("pull_request closed -> {target_state}"),
    })
}
