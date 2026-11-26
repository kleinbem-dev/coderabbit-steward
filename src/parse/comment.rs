use crate::parse::{is_coderabbit_login, parse_timestamp};
use jiff::Timestamp;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotMessage {
    RateLimited {
        at: Timestamp,
        minutes_until_free: u32,
    },
    ProbeRateLimited {
        at: Timestamp,
        minutes_until_free: u32,
    },
    ProbeAvailableNow {
        at: Timestamp,
    },
    SkippedBotAuthor {
        at: Timestamp,
    },
    Skipped {
        at: Timestamp,
    },
    ReviewSummary {
        at: Timestamp,
        actionable_comments: Option<u32>,
        files_processed: Option<u32>,
        limit: Option<u32>,
        remaining_after: Option<u32>,
        no_actionable: bool,
    },
    ReviewFinished {
        at: Timestamp,
    },
    StatusComment {
        at: Timestamp,
    },
    Unrecognised {
        at: Timestamp,
    },
}

#[derive(Debug, Deserialize)]
struct RawComment {
    user: Option<RawUser>,
    body: Option<String>,
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    login: String,
}

static RE_NEXT_INCLUDED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Next included review available in (\d+) minutes?").expect("valid regex")
});
static RE_PROBE_MORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)More reviews will be available in (\d+) minutes?").expect("valid regex")
});
static RE_ACTIONABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Actionable comments posted:\s*(\d+)").expect("valid regex"));
static RE_FILES_SELECTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Files selected for processing:\s*(\d+)").expect("valid regex"));
static RE_FOOTER_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"up to (\d+) included reviews? per hour;\s*(\d+) remains? after this review")
        .expect("valid regex")
});

/// Parse a GitHub issue comment JSON payload. Returns `None` if not authored by CodeRabbit.
pub fn parse_issue_comment(payload: &str) -> Option<BotMessage> {
    let raw: RawComment = serde_json::from_str(payload).ok()?;
    let login = raw.user.as_ref()?.login.as_str();
    if !is_coderabbit_login(login) {
        return None;
    }
    let body = raw.body.as_deref().unwrap_or("");
    let at = raw
        .created_at
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(Timestamp::UNIX_EPOCH);
    Some(classify_bot_body(body, at))
}

pub fn classify_bot_body(body: &str, at: Timestamp) -> BotMessage {
    let trimmed = body.trim();

    if let Some(caps) = RE_NEXT_INCLUDED.captures(body)
        && let Ok(mins) = caps[1].parse::<u32>()
    {
        return BotMessage::RateLimited {
            at,
            minutes_until_free: mins,
        };
    }

    if let Some(caps) = RE_PROBE_MORE.captures(body)
        && let Ok(mins) = caps[1].parse::<u32>()
    {
        return BotMessage::ProbeRateLimited {
            at,
            minutes_until_free: mins,
        };
    }

    if body.contains("Reviews are available now") {
        return BotMessage::ProbeAvailableNow { at };
    }

    if body.contains("authored by a bot without an assigned CodeRabbit review seat") {
        return BotMessage::SkippedBotAuthor { at };
    }

    if trimmed == "Review finished." || body.contains("Review finished.") {
        return BotMessage::ReviewFinished { at };
    }

    let actionable = RE_ACTIONABLE
        .captures(body)
        .and_then(|c| c[1].parse::<u32>().ok());
    let files_processed = RE_FILES_SELECTED
        .captures(body)
        .and_then(|c| c[1].parse::<u32>().ok());
    let (limit, remaining_after) = RE_FOOTER_LIMIT
        .captures(body)
        .map(|c| (c[1].parse::<u32>().ok(), c[2].parse::<u32>().ok()))
        .unwrap_or((None, None));
    let no_actionable = body.contains("No actionable comments were generated in the recent review");

    if actionable.is_some() || limit.is_some() || no_actionable {
        return BotMessage::ReviewSummary {
            at,
            actionable_comments: actionable.or(if no_actionable { Some(0) } else { None }),
            files_processed,
            limit,
            remaining_after,
            no_actionable,
        };
    }

    if body.contains("## Review skipped") || body.contains("Review skipped.") {
        return BotMessage::Skipped { at };
    }

    if body.contains("<!-- This is an auto-generated comment: summarize by coderabbit.ai -->")
        || body.contains("Merge Risk:")
        || body.contains("Pre-merge checks")
        || body.contains("Review rate limited.")
    {
        return BotMessage::StatusComment { at };
    }

    BotMessage::Unrecognised { at }
}
