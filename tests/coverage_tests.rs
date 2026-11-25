use axum::body::Body;
use axum::http::{Request, StatusCode};
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::coverage::{
    CommitFileChange, CommitVerdict, apply_commit_changes, is_excluded_path, is_risky_path,
    summarize,
};
use coderabbit_steward::db::Db;
use jiff::Timestamp;
use serde_json::Value;
use std::collections::HashMap;
use tower::ServiceExt;

#[test]
fn path_filters_classify_excluded_and_risky_paths() {
    assert!(is_excluded_path("flake.lock"));
    assert!(is_excluded_path("Cargo.lock"));
    assert!(is_excluded_path("web/pnpm-lock.yaml"));
    assert!(is_excluded_path("web/node_modules/foo/index.js"));
    assert!(is_excluded_path("assets/logo.svg"));
    assert!(!is_excluded_path("src/main.rs"));

    assert!(is_risky_path(".github/workflows/ci.yml"));
    assert!(is_risky_path("infra/main.tf"));
    assert!(is_risky_path("hosts/nasbook/secrets.nix"));
    assert!(is_risky_path("modules/firewall.nix"));
    assert!(!is_risky_path("README.md"));
}

#[test]
fn coverage_tracks_reviewed_pushed_skipped_and_removed_files() {
    let mut map = HashMap::new();
    let t0: Timestamp = "2026-11-20T10:00:00Z".parse().unwrap();
    let t1: Timestamp = "2026-11-22T10:00:00Z".parse().unwrap();
    let now: Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();

    // 1. Reviewed PR covers src/lib.rs and flake.lock (excluded)
    apply_commit_changes(
        &mut map,
        &[
            CommitFileChange {
                path: "src/lib.rs".into(),
                blob_sha: Some("blob1".into()),
                lines_changed: 50,
                removed: false,
            },
            CommitFileChange {
                path: "flake.lock".into(),
                blob_sha: Some("lock1".into()),
                lines_changed: 200,
                removed: false,
            },
        ],
        &CommitVerdict::Reviewed {
            by: "pr".into(),
            commit_sha: "abc1111".into(),
        },
    );

    // 2. Direct push / unreviewed commit modifies src/main.rs and .github/workflows/ci.yml
    apply_commit_changes(
        &mut map,
        &[
            CommitFileChange {
                path: "src/main.rs".into(),
                blob_sha: Some("blob2".into()),
                lines_changed: 40,
                removed: false,
            },
            CommitFileChange {
                path: ".github/workflows/ci.yml".into(),
                blob_sha: Some("blob3".into()),
                lines_changed: 20,
                removed: false,
            },
        ],
        &CommitVerdict::Unreviewed { at: t0 },
    );

    // 3. Follow-up unreviewed push adds more lines to src/main.rs (preserves t0 as unreviewed_since)
    apply_commit_changes(
        &mut map,
        &[CommitFileChange {
            path: "src/main.rs".into(),
            blob_sha: Some("blob2b".into()),
            lines_changed: 10,
            removed: false,
        }],
        &CommitVerdict::Unreviewed { at: t1 },
    );

    let files: Vec<_> = map.values().cloned().collect();
    let summary = summarize("kleinbem/demo", &files, now);

    // 3 non-excluded files: src/lib.rs (reviewed), src/main.rs & .github/workflows/ci.yml (unreviewed)
    assert_eq!(summary.total_files, 3);
    assert_eq!(summary.reviewed_files, 1);
    assert_eq!(summary.unreviewed_files.len(), 2);
    assert!((summary.percentage - 33.3).abs() < 0.2);

    // Risky .github/workflows/ci.yml must rank above src/main.rs
    assert_eq!(summary.unreviewed_files[0].path, ".github/workflows/ci.yml");
    assert!(summary.unreviewed_files[0].risky);
    assert_eq!(summary.unreviewed_files[1].path, "src/main.rs");
    assert_eq!(summary.unreviewed_files[1].unreviewed_lines, 50);
    assert_eq!(
        summary.unreviewed_files[1].unreviewed_since.as_deref(),
        Some("2026-11-20T10:00:00Z")
    );

    // 4. Removing .github/workflows/ci.yml drops it from the map
    apply_commit_changes(
        &mut map,
        &[CommitFileChange {
            path: ".github/workflows/ci.yml".into(),
            blob_sha: None,
            lines_changed: 0,
            removed: true,
        }],
        &CommitVerdict::Unreviewed { at: now },
    );
    let summary2 = summarize("kleinbem/demo", &map.values().cloned().collect::<Vec<_>>(), now);
    assert_eq!(summary2.total_files, 2);
    assert_eq!(summary2.reviewed_files, 1);
    assert!((summary2.percentage - 50.0).abs() < 0.1);
}

#[tokio::test]
async fn coverage_api_and_fleet_budget_report_real_percentage() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("cov.db").display());
    let db = Db::connect(&db_url).await.unwrap();
    db.upsert_repo("kleinbem", "demo", "public", 1, "main")
        .await
        .unwrap();

    let mut map = HashMap::new();
    let now: Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
    apply_commit_changes(
        &mut map,
        &[CommitFileChange {
            path: "src/a.rs".into(),
            blob_sha: Some("b1".into()),
            lines_changed: 10,
            removed: false,
        }],
        &CommitVerdict::Reviewed {
            by: "pr".into(),
            commit_sha: "c1".into(),
        },
    );
    apply_commit_changes(
        &mut map,
        &[CommitFileChange {
            path: "src/b.rs".into(),
            blob_sha: Some("b2".into()),
            lines_changed: 25,
            removed: false,
        }],
        &CommitVerdict::Unreviewed { at: now },
    );
    for state in map.values() {
        db.upsert_file_coverage("kleinbem/demo", state).await.unwrap();
    }
    db.set_scan_cursor("kleinbem/demo", "c2").await.unwrap();
    assert_eq!(
        db.get_scan_cursor("kleinbem/demo").await.unwrap().as_deref(),
        Some("c2")
    );

    let router = build_router(
        AppState {
            db,
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: None,
            ntfy: None,
            revue: None,
        },
        None,
    );

    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/v1/coverage/kleinbem/demo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["total_files"], 2);
    assert_eq!(body["reviewed_files"], 1);
    assert_eq!(body["percentage"], 50.0);
    assert_eq!(body["unreviewed_files"][0]["path"], "src/b.rs");

    let resp = router
        .oneshot(
            Request::get("/api/v1/budget")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let fleet: Vec<Value> =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(fleet[0]["coverage_pct"], 50.0);
}
