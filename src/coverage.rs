use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverageState {
    pub path: String,
    pub reviewed_blob: Option<String>,
    pub reviewed_commit: Option<String>,
    pub by: Option<String>,
    pub unreviewed_lines: i64,
    pub unreviewed_since: Option<String>,
    pub excluded: bool,
}

#[derive(Debug, Clone)]
pub struct CommitFileChange {
    pub path: String,
    pub blob_sha: Option<String>,
    pub lines_changed: u32,
    pub removed: bool,
}

#[derive(Debug, Clone)]
pub enum CommitVerdict {
    Reviewed { by: String, commit_sha: String },
    Unreviewed { at: Timestamp },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnreviewedFileItem {
    pub path: String,
    pub unreviewed_lines: i64,
    pub unreviewed_since: Option<String>,
    pub priority: f64,
    pub risky: bool,
    pub by: Option<String>,
    pub reviewed_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub repo: String,
    pub percentage: f64,
    pub total_files: usize,
    pub reviewed_files: usize,
    pub unreviewed_files: Vec<UnreviewedFileItem>,
}

/// Returns true for lockfiles, build artifacts, and binary assets excluded from review coverage.
pub fn is_excluded_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    if matches!(
        filename,
        "flake.lock"
            | "cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "go.sum"
            | "poetry.lock"
            | "uv.lock"
    ) {
        return true;
    }
    let excluded_prefixes = [
        "node_modules/",
        "target/",
        "dist/",
        "build/",
        ".svelte-kit/",
        ".git/",
        ".jj/",
    ];
    if excluded_prefixes.iter().any(|p| lower.starts_with(p) || lower.contains(&format!("/{p}"))) {
        return true;
    }
    let excluded_exts = [
        ".min.js", ".min.css", ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".svg", ".woff",
        ".woff2", ".ttf", ".pdf", ".wasm", ".age", ".sops",
    ];
    excluded_exts.iter().any(|ext| lower.ends_with(ext))
}

/// Matches fleet-lint's high-risk path patterns (CI, Terraform, secrets, auth, firewall, TLS).
pub fn is_risky_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with(".github/")
        || lower.ends_with(".tf")
        || lower.ends_with(".tfvars")
        || lower.ends_with(".sops.yaml")
    {
        return true;
    }
    let risky_keywords = [
        "secret", "ssh", "firewall", "network", "auth", "tls", "cert", "policy", "security",
    ];
    risky_keywords.iter().any(|kw| lower.contains(kw))
}

/// Apply one commit's file changes to an in-memory coverage map.
pub fn apply_commit_changes(
    map: &mut HashMap<String, FileCoverageState>,
    changes: &[CommitFileChange],
    verdict: &CommitVerdict,
) {
    for ch in changes {
        if ch.removed {
            map.remove(&ch.path);
            continue;
        }
        if is_excluded_path(&ch.path) {
            map.insert(
                ch.path.clone(),
                FileCoverageState {
                    path: ch.path.clone(),
                    reviewed_blob: ch.blob_sha.clone(),
                    reviewed_commit: None,
                    by: None,
                    unreviewed_lines: 0,
                    unreviewed_since: None,
                    excluded: true,
                },
            );
            continue;
        }

        match verdict {
            CommitVerdict::Reviewed { by, commit_sha } => {
                map.insert(
                    ch.path.clone(),
                    FileCoverageState {
                        path: ch.path.clone(),
                        reviewed_blob: ch.blob_sha.clone(),
                        reviewed_commit: Some(commit_sha.clone()),
                        by: Some(by.clone()),
                        unreviewed_lines: 0,
                        unreviewed_since: None,
                        excluded: false,
                    },
                );
            }
            CommitVerdict::Unreviewed { at } => {
                let entry = map
                    .entry(ch.path.clone())
                    .or_insert_with(|| FileCoverageState {
                        path: ch.path.clone(),
                        reviewed_blob: None,
                        reviewed_commit: None,
                        by: None,
                        unreviewed_lines: 0,
                        unreviewed_since: None,
                        excluded: false,
                    });
                entry.excluded = false;
                entry.unreviewed_lines += i64::from(ch.lines_changed.max(1));
                if entry.unreviewed_since.is_none() {
                    entry.unreviewed_since = Some(at.to_string());
                }
            }
        }
    }
}

/// Compute review priority for an unreviewed file: `risk × never_reviewed_boost × ln(1 + lines) × age_days`.
pub fn priority_score(file: &FileCoverageState, now: Timestamp) -> f64 {
    let risk = if is_risky_path(&file.path) { 3.0 } else { 1.0 };
    let never_reviewed_boost = if file.reviewed_commit.is_none() {
        2.0
    } else {
        1.0
    };
    let lines = file.unreviewed_lines.max(1) as f64;
    let age_days = file
        .unreviewed_since
        .as_deref()
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map(|ts| (now.duration_since(ts).as_secs().max(0) as f64) / 86_400.0)
        .unwrap_or(1.0)
        .max(0.5);
    let score = risk * never_reviewed_boost * (1.0 + lines).ln() * age_days;
    (score * 100.0).round() / 100.0
}

/// Summarize coverage percentage and prioritized unreviewed files for a repository.
pub fn summarize(repo: &str, files: &[FileCoverageState], now: Timestamp) -> CoverageSummary {
    let non_excluded: Vec<&FileCoverageState> = files.iter().filter(|f| !f.excluded).collect();
    let total_files = non_excluded.len();
    let mut reviewed_files = 0usize;
    let mut unreviewed_files = Vec::new();

    for f in non_excluded {
        if f.unreviewed_lines == 0 && f.reviewed_commit.is_some() {
            reviewed_files += 1;
        } else {
            unreviewed_files.push(UnreviewedFileItem {
                path: f.path.clone(),
                unreviewed_lines: f.unreviewed_lines,
                unreviewed_since: f.unreviewed_since.clone(),
                priority: priority_score(f, now),
                risky: is_risky_path(&f.path),
                by: f.by.clone(),
                reviewed_commit: f.reviewed_commit.clone(),
            });
        }
    }

    unreviewed_files.sort_by(|a, b| {
        b.priority
            .total_cmp(&a.priority)
            .then_with(|| a.path.cmp(&b.path))
    });

    let percentage = if total_files == 0 {
        0.0
    } else {
        ((reviewed_files as f64 / total_files as f64) * 1000.0).round() / 10.0
    };

    CoverageSummary {
        repo: repo.to_string(),
        percentage,
        total_files,
        reviewed_files,
        unreviewed_files,
    }
}
