use crate::parse::comment::{BotMessage, classify_bot_body};
use crate::parse::{is_coderabbit_login, parse_timestamp};
use jiff::Timestamp;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RawReview {
    user: Option<RawUser>,
    body: Option<String>,
    submitted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    login: String,
}

/// Parse a GitHub pull request review JSON payload. Returns `None` if not authored by CodeRabbit.
pub fn parse_pr_review(payload: &str) -> Option<BotMessage> {
    let raw: RawReview = serde_json::from_str(payload).ok()?;
    let login = raw.user.as_ref()?.login.as_str();
    if !is_coderabbit_login(login) {
        return None;
    }
    let body = raw.body.as_deref().unwrap_or("");
    let at = raw
        .submitted_at
        .as_deref()
        .and_then(parse_timestamp)
        .unwrap_or(Timestamp::UNIX_EPOCH);
    Some(classify_bot_body(body, at))
}
