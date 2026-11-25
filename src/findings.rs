use crate::db::FindingRow;
use crate::parse::is_coderabbit_login;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFinding {
    pub path: String,
    pub line: Option<i64>,
    pub severity: String,
    pub category: String,
    pub title: String,
    pub body: String,
    pub code_quote: String,
}

static RE_BOLD_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*([^*\n]{4,140})\*\*").expect("valid regex"));

static RE_OUTSIDE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)`([^`\n]+)`(?:\s*\((\d+)(?:-\d+)?\))?:\s*\n+(.+?)(?:\n---|\z)")
        .expect("valid regex")
});

/// Deterministic SHA-256 fingerprint so repeated reviews inherit existing triage status.
pub fn compute_fingerprint(repo: &str, path: &str, title: &str, code_quote: &str) -> String {
    let norm_title = title
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let norm_quote = code_quote
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(norm_title.as_bytes());
    hasher.update(b"\0");
    hasher.update(norm_quote.as_bytes());
    hex::encode(hasher.finalize())
}

/// Extract a stable code quote from a GitHub diff hunk, excluding volatile `@@` hunk headers.
fn stable_hunk_quote(diff_hunk: &str) -> String {
    let code_lines: Vec<&str> = diff_hunk
        .lines()
        .filter(|l| !l.starts_with("@@"))
        .map(|l| l.trim_start_matches(['+', '-', ' ']).trim())
        .filter(|l| !l.is_empty())
        .collect();
    let start = code_lines.len().saturating_sub(3);
    code_lines[start..].join("\n")
}

/// Read severity from explicit CodeRabbit badge header lines rather than explanation prose.
fn detect_severity_and_category(body: &str) -> (String, String) {
    let header: String = body
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with("<!--"))
        .take(3)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();

    let severity = if header.contains("**critical**") || header.contains("🔴") {
        "critical"
    } else if header.contains("**major**")
        || header.contains("🟠")
        || header.contains("potential issue")
    {
        "major"
    } else if header.contains("**nitpick**") || header.contains("🧹") {
        "nitpick"
    } else {
        "minor"
    };

    let category = if header.contains("security") {
        "security"
    } else if header.contains("correctness") || header.contains("potential issue") {
        "correctness"
    } else if header.contains("performance") {
        "performance"
    } else if header.contains("refactor") || header.contains("maintainability") {
        "maintainability"
    } else {
        "general"
    };
    (severity.to_string(), category.to_string())
}

fn extract_title(body: &str, fallback_path: &str) -> String {
    for line in body.lines() {
        let trimmed = line.trim();
        // Skip CodeRabbit severity/category badge header lines separated by middle dots.
        if trimmed.contains('·') {
            continue;
        }
        if let Some(caps) = RE_BOLD_TITLE.captures(trimmed) {
            let candidate = caps[1].trim();
            let lower = candidate.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "critical" | "major" | "minor" | "nitpick" | "quick win" | "heavy lift"
            ) {
                continue;
            }
            return candidate.to_string();
        }
    }
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("<!--") && !l.starts_with('<') && !l.contains('·'))
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_else(|| format!("Finding in {fallback_path}"))
}

/// Parse a GitHub pull request inline review comment JSON object authored by CodeRabbit.
pub fn parse_review_comment(comment: &Value) -> Option<ParsedFinding> {
    let login = comment
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)?;
    if !is_coderabbit_login(login) {
        return None;
    }
    let path = comment.get("path").and_then(Value::as_str)?.to_string();
    let body = comment.get("body").and_then(Value::as_str)?.trim();
    if body.is_empty() {
        return None;
    }
    let line = comment
        .get("line")
        .and_then(Value::as_i64)
        .or_else(|| comment.get("original_line").and_then(Value::as_i64));
    let raw_hunk = comment
        .get("diff_hunk")
        .and_then(Value::as_str)
        .unwrap_or("");
    let code_quote = stable_hunk_quote(raw_hunk);
    let (severity, category) = detect_severity_and_category(body);
    let title = extract_title(body, &path);

    Some(ParsedFinding {
        path,
        line,
        severity,
        category,
        title,
        body: body.to_string(),
        code_quote,
    })
}

/// Parse outside-diff-range findings from a CodeRabbit review summary body.
pub fn parse_outside_diff_findings(review_body: &str) -> Vec<ParsedFinding> {
    if !review_body.contains("Outside diff range") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for caps in RE_OUTSIDE_BLOCK.captures_iter(review_body) {
        let path = caps[1].trim().to_string();
        let line = caps.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
        let body = caps[3].trim().to_string();
        if body.is_empty() {
            continue;
        }
        let (severity, category) = detect_severity_and_category(&body);
        let title = extract_title(&body, &path);
        // Anchor outside-diff findings to a fenced code snippet if present, else line number,
        // so two occurrences with the same title in one file remain distinct.
        let code_quote = body
            .split("```")
            .nth(1)
            .map(|blk| {
                blk.lines()
                    .skip(1)
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| line.map(|l| format!("line:{l}")).unwrap_or_default());
        out.push(ParsedFinding {
            path,
            line,
            severity,
            category,
            title,
            body,
            code_quote,
        });
    }
    out
}

/// Render the rolling `CodeRabbit findings` GitHub issue body grouped by severity.
pub fn render_findings_issue_body(repo: &str, open_findings: &[FindingRow]) -> String {
    let mut out = String::new();
    out.push_str("## Open CodeRabbit Findings\n\n");
    out.push_str(&format!(
        "Tracked automatically by `coderabbit-steward` for `{repo}`. \
         Triage via `cr-steward findings --repo {repo} --status new --json` or the dashboard.\n\n"
    ));

    if open_findings.is_empty() {
        out.push_str("_No open findings._\n");
        return out;
    }

    for sev in ["critical", "major", "minor", "nitpick"] {
        let matching: Vec<&FindingRow> = open_findings
            .iter()
            .filter(|f| f.severity.eq_ignore_ascii_case(sev))
            .collect();
        if matching.is_empty() {
            continue;
        }
        out.push_str(&format!("### {} ({})\n\n", sev.to_ascii_uppercase(), matching.len()));
        for f in matching {
            let loc = match f.line {
                Some(l) => format!("`{}:{}`", f.path, l),
                None => format!("`{}`", f.path),
            };
            out.push_str(&format!(
                "- **[#{}]** {} — {} (`{}`)\n",
                f.id, loc, f.title, f.status
            ));
        }
        out.push('\n');
    }
    out
}
