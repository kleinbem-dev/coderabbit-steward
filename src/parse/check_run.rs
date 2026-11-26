use crate::parse::parse_timestamp;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckRunOutcome {
    InProgress {
        started_at: Timestamp,
    },
    Reviewed {
        started_at: Timestamp,
        completed_at: Option<Timestamp>,
    },
    RateLimited {
        started_at: Timestamp,
        completed_at: Option<Timestamp>,
    },
    SkippedLabel {
        started_at: Timestamp,
    },
    SkippedIncremental {
        started_at: Timestamp,
    },
    SkippedBotAuthor {
        started_at: Timestamp,
    },
    SkippedOther {
        started_at: Timestamp,
        title: String,
    },
    Unrecognised {
        status: String,
        conclusion: Option<String>,
        title: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct RawCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    output: Option<RawCheckOutput>,
    app: Option<RawCheckApp>,
}

#[derive(Debug, Deserialize)]
struct RawCheckOutput {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCheckApp {
    slug: Option<String>,
}

/// Parse a GitHub check_run JSON payload. Returns `None` if it is not a CodeRabbit check run.
pub fn parse_check_run(payload: &str) -> Option<CheckRunOutcome> {
    let raw: RawCheckRun = serde_json::from_str(payload).ok()?;
    let is_cr = raw.name == "CodeRabbit"
        || raw
            .app
            .as_ref()
            .and_then(|a| a.slug.as_deref())
            .is_some_and(|s| s == "coderabbitai");
    if !is_cr {
        return None;
    }

    let started_at = raw.started_at.as_deref().and_then(parse_timestamp);
    let completed_at = raw.completed_at.as_deref().and_then(parse_timestamp);
    let title = raw
        .output
        .as_ref()
        .and_then(|o| o.title.as_deref())
        .unwrap_or("")
        .trim()
        .to_string();

    if raw.status == "in_progress" || raw.status == "queued" {
        return Some(match started_at {
            Some(ts) => CheckRunOutcome::InProgress { started_at: ts },
            None => CheckRunOutcome::Unrecognised {
                status: raw.status,
                conclusion: raw.conclusion,
                title: (!title.is_empty()).then_some(title),
            },
        });
    }

    if raw.status != "completed" {
        return Some(CheckRunOutcome::Unrecognised {
            status: raw.status,
            conclusion: raw.conclusion,
            title: (!title.is_empty()).then_some(title),
        });
    }

    let Some(started_at) = started_at else {
        return Some(CheckRunOutcome::Unrecognised {
            status: raw.status,
            conclusion: raw.conclusion,
            title: (!title.is_empty()).then_some(title),
        });
    };

    match raw.conclusion.as_deref() {
        Some("success") if title.eq_ignore_ascii_case("Finished") => {
            Some(CheckRunOutcome::Reviewed {
                started_at,
                completed_at,
            })
        }
        Some("neutral") if title.to_ascii_lowercase().contains("rate limited") => {
            Some(CheckRunOutcome::RateLimited {
                started_at,
                completed_at,
            })
        }
        Some("skipped") => {
            let lower = title.to_ascii_lowercase();
            if lower.contains("bot user not eligible") {
                Some(CheckRunOutcome::SkippedBotAuthor { started_at })
            } else if lower.contains("incremental") {
                Some(CheckRunOutcome::SkippedIncremental { started_at })
            } else if lower.contains("label") {
                Some(CheckRunOutcome::SkippedLabel { started_at })
            } else {
                Some(CheckRunOutcome::SkippedOther { started_at, title })
            }
        }
        _ => Some(CheckRunOutcome::Unrecognised {
            status: raw.status,
            conclusion: raw.conclusion,
            title: (!title.is_empty()).then_some(title),
        }),
    }
}
