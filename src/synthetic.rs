use crate::coverage::{FileCoverageState, priority_score};
use jiff::Timestamp;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone)]
pub struct ProactiveBatch {
    pub area: String,
    pub files: Vec<FileCoverageState>,
    pub total_lines: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitTreeEntry {
    pub path: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub sha: String,
}

/// Top-level directory area for grouping proactive review batches.
pub fn area_for_path(path: &str) -> String {
    match path.split_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => "root".to_string(),
    }
}

/// Pick the highest-priority directory area and cap the batch by `max_files` / `max_lines`.
pub fn select_proactive_batch(
    files: &[FileCoverageState],
    now: Timestamp,
    max_files: usize,
    max_lines: i64,
) -> Option<ProactiveBatch> {
    let mut by_area: BTreeMap<String, Vec<(f64, FileCoverageState)>> = BTreeMap::new();
    for f in files {
        if f.excluded {
            continue;
        }
        if f.unreviewed_lines == 0 && f.reviewed_commit.is_some() {
            continue;
        }
        let area = area_for_path(&f.path);
        let score = priority_score(f, now);
        by_area.entry(area).or_default().push((score, f.clone()));
    }

    let (best_area, mut candidates) = by_area
        .into_iter()
        .max_by(|(area_a, list_a), (area_b, list_b)| {
            let sum_a: f64 = list_a.iter().map(|(s, _)| *s).sum();
            let sum_b: f64 = list_b.iter().map(|(s, _)| *s).sum();
            sum_a.total_cmp(&sum_b).then_with(|| area_b.cmp(area_a))
        })?;

    candidates.sort_by(|(sa, fa), (sb, fb)| sb.total_cmp(sa).then_with(|| fa.path.cmp(&fb.path)));

    let mut selected = Vec::new();
    let mut total_lines = 0i64;
    for (_, f) in candidates {
        let file_lines = f.unreviewed_lines.max(1);
        if !selected.is_empty() && (selected.len() >= max_files || total_lines + file_lines > max_lines)
        {
            break;
        }
        total_lines += file_lines;
        selected.push(f);
    }

    if selected.is_empty() {
        None
    } else {
        Some(ProactiveBatch {
            area: best_area,
            files: selected,
            total_lines,
        })
    }
}

/// Split all non-excluded files in a repo into area-grouped batches for full-repo review mode,
/// treating every file as never reviewed (`reviewed_blob = None`).
pub fn split_full_repo_batches(
    files: &[FileCoverageState],
    max_files: usize,
) -> Vec<ProactiveBatch> {
    let cap = max_files.max(1);
    let mut by_area: BTreeMap<String, Vec<FileCoverageState>> = BTreeMap::new();
    for f in files {
        if f.excluded {
            continue;
        }
        let mut full_file = f.clone();
        full_file.reviewed_blob = None;
        full_file.reviewed_commit = None;
        by_area
            .entry(area_for_path(&f.path))
            .or_default()
            .push(full_file);
    }

    let mut batches = Vec::new();
    for (area, mut area_files) in by_area {
        area_files.sort_by(|a, b| a.path.cmp(&b.path));
        for (idx, chunk) in area_files.chunks(cap).enumerate() {
            let chunk_area = if area_files.len() > cap {
                format!("{area}-p{}", idx + 1)
            } else {
                area.clone()
            };
            let total_lines = chunk.iter().map(|f| f.unreviewed_lines.max(1)).sum();
            batches.push(ProactiveBatch {
                area: chunk_area,
                files: chunk.to_vec(),
                total_lines,
            });
        }
    }
    batches
}

/// Build a `ProactiveBatch` from an explicit queued path list (`full` or `proactive` queue item).
pub fn batch_from_paths(
    paths: &[String],
    files: &[FileCoverageState],
    force_full: bool,
) -> Option<ProactiveBatch> {
    if paths.is_empty() {
        return None;
    }
    let by_path: HashMap<&str, &FileCoverageState> =
        files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut selected = Vec::with_capacity(paths.len());
    for p in paths {
        let mut st = by_path.get(p.as_str()).map(|f| (*f).clone()).unwrap_or(FileCoverageState {
            path: p.clone(),
            reviewed_blob: None,
            reviewed_commit: None,
            by: None,
            unreviewed_lines: 1,
            unreviewed_since: None,
            excluded: false,
        });
        if force_full {
            st.reviewed_blob = None;
            st.reviewed_commit = None;
        }
        selected.push(st);
    }
    let area = area_for_path(&selected[0].path);
    let total_lines = selected.iter().map(|f| f.unreviewed_lines.max(1)).sum();
    Some(ProactiveBatch {
        area,
        files: selected,
        total_lines,
    })
}

/// Build the synthetic base tree from the current default-branch entries (`path, mode, type, sha`):
/// - Unchanged blobs and submodule gitlinks (`commit`) keep their current entry.
/// - Batch files with a `reviewed_blob` revert to that blob (diff = only unreviewed changes).
/// - Never-reviewed batch files (`reviewed_blob == None`) are omitted (diff = whole file).
pub fn build_base_tree_entries(
    head_entries: &[(String, String, String, String)],
    batch: &[FileCoverageState],
) -> Vec<GitTreeEntry> {
    let batch_map: HashMap<&str, Option<&str>> = batch
        .iter()
        .map(|f| (f.path.as_str(), f.reviewed_blob.as_deref()))
        .collect();

    let mut entries = Vec::with_capacity(head_entries.len());
    for (path, mode, entry_type, sha) in head_entries {
        if entry_type != "blob" {
            entries.push(GitTreeEntry {
                path: path.clone(),
                mode: mode.clone(),
                entry_type: entry_type.clone(),
                sha: sha.clone(),
            });
            continue;
        }
        match batch_map.get(path.as_str()) {
            None => entries.push(GitTreeEntry {
                path: path.clone(),
                mode: mode.clone(),
                entry_type: entry_type.clone(),
                sha: sha.clone(),
            }),
            Some(Some(reviewed_sha)) => entries.push(GitTreeEntry {
                path: path.clone(),
                mode: mode.clone(),
                entry_type: entry_type.clone(),
                sha: (*reviewed_sha).to_string(),
            }),
            Some(None) => {
                // Omit never-reviewed file from base tree so it shows up whole in the diff.
            }
        }
    }
    entries
}
