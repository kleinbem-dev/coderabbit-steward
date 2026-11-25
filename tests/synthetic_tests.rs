use coderabbit_steward::coverage::FileCoverageState;
use coderabbit_steward::synthetic::{area_for_path, build_base_tree_entries, select_proactive_batch};
use jiff::Timestamp;

#[test]
fn selects_highest_priority_area_and_builds_delta_base_tree() {
    let now: Timestamp = "2026-11-25T00:00:00Z".parse().unwrap();
    let files = vec![
        FileCoverageState {
            path: "README.md".into(),
            reviewed_blob: Some("blob_readme".into()),
            reviewed_commit: Some("c1".into()),
            by: Some("pr".into()),
            unreviewed_lines: 0,
            unreviewed_since: None,
            excluded: false,
        },
        FileCoverageState {
            path: "src/auth.rs".into(),
            reviewed_blob: Some("blob_auth_old".into()),
            reviewed_commit: Some("c1".into()),
            by: Some("pr".into()),
            unreviewed_lines: 80,
            unreviewed_since: Some("2026-11-20T00:00:00Z".into()),
            excluded: false,
        },
        FileCoverageState {
            path: "src/new_mod.rs".into(),
            reviewed_blob: None,
            reviewed_commit: None,
            by: None,
            unreviewed_lines: 120,
            unreviewed_since: Some("2026-11-21T00:00:00Z".into()),
            excluded: false,
        },
        FileCoverageState {
            path: "docs/guide.md".into(),
            reviewed_blob: None,
            reviewed_commit: None,
            by: None,
            unreviewed_lines: 15,
            unreviewed_since: Some("2026-11-24T00:00:00Z".into()),
            excluded: false,
        },
    ];

    assert_eq!(area_for_path("src/auth.rs"), "src");
    assert_eq!(area_for_path("README.md"), "root");

    let batch = select_proactive_batch(&files, now, 50, 2000).expect("batch should be selected");
    assert_eq!(batch.area, "src");
    assert_eq!(batch.files.len(), 2);

    let head_blobs = vec![
        ("README.md".into(), "100644".into(), "blob_readme".into()),
        ("src/auth.rs".into(), "100644".into(), "blob_auth_new".into()),
        ("src/new_mod.rs".into(), "100644".into(), "blob_new_mod".into()),
        ("docs/guide.md".into(), "100644".into(), "blob_guide".into()),
    ];

    let base_tree = build_base_tree_entries(&head_blobs, &batch.files);
    // Unchanged files stay at head blob for full context
    assert!(base_tree.iter().any(|e| e.path == "README.md" && e.sha == "blob_readme"));
    assert!(base_tree.iter().any(|e| e.path == "docs/guide.md" && e.sha == "blob_guide"));
    // Previously reviewed batch file reverts to reviewed_blob (diff = only unreviewed delta)
    assert!(base_tree.iter().any(|e| e.path == "src/auth.rs" && e.sha == "blob_auth_old"));
    // Never-reviewed batch file is omitted from base tree (diff = whole file)
    assert!(!base_tree.iter().any(|e| e.path == "src/new_mod.rs"));
}
