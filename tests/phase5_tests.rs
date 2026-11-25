use axum::body::Body;
use axum::http::{Request, StatusCode};
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::coverage::FileCoverageState;
use coderabbit_steward::db::Db;
use coderabbit_steward::synthetic::{batch_from_paths, split_full_repo_batches};
use serde_json::Value;
use tower::ServiceExt;

#[test]
fn split_full_repo_batches_chunks_by_area_and_clears_reviewed_blobs() {
    let files = vec![
        FileCoverageState {
            path: "src/a.rs".into(),
            reviewed_blob: Some("old1".into()),
            reviewed_commit: Some("c1".into()),
            by: Some("pr".into()),
            unreviewed_lines: 0,
            unreviewed_since: None,
            excluded: false,
        },
        FileCoverageState {
            path: "src/b.rs".into(),
            reviewed_blob: Some("old2".into()),
            reviewed_commit: Some("c1".into()),
            by: Some("pr".into()),
            unreviewed_lines: 0,
            unreviewed_since: None,
            excluded: false,
        },
        FileCoverageState {
            path: "src/c.rs".into(),
            reviewed_blob: Some("old3".into()),
            reviewed_commit: Some("c1".into()),
            by: Some("pr".into()),
            unreviewed_lines: 0,
            unreviewed_since: None,
            excluded: false,
        },
        FileCoverageState {
            path: "flake.lock".into(),
            reviewed_blob: None,
            reviewed_commit: None,
            by: None,
            unreviewed_lines: 0,
            unreviewed_since: None,
            excluded: true,
        },
    ];

    let batches = split_full_repo_batches(&files, 2);
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].area, "src-p1");
    assert_eq!(batches[0].files.len(), 2);
    assert!(batches[0].files.iter().all(|f| f.reviewed_blob.is_none()));
    assert_eq!(batches[1].area, "src-p2");
    assert_eq!(batches[1].files.len(), 1);

    let rebuilt = batch_from_paths(&["src/a.rs".into()], &files, true).unwrap();
    assert!(rebuilt.files[0].reviewed_blob.is_none());
}

#[tokio::test]
async fn full_review_enqueues_batches_and_revue_endpoint_updates_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("p5.db").display());
    let db = Db::connect(&db_url).await.unwrap();
    db.upsert_repo("kleinbem", "demo", "public", 1, "main")
        .await
        .unwrap();

    for path in ["src/a.rs", "docs/readme.md"] {
        db.upsert_file_coverage(
            "kleinbem/demo",
            &FileCoverageState {
                path: path.into(),
                reviewed_blob: None,
                reviewed_commit: None,
                by: None,
                unreviewed_lines: 20,
                unreviewed_since: Some("2026-11-24T00:00:00Z".into()),
                excluded: false,
            },
        )
        .await
        .unwrap();
    }

    let router = build_router(
        AppState {
            db: db.clone(),
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: None,
            ntfy: None,
            revue: None,
        },
        None,
    );

    // 1. Full repo review splits into 2 area batches (docs + src)
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/reviews")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"repo":"kleinbem/demo","full":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let queue = db.list_queue(Some("kleinbem/demo")).await.unwrap();
    assert_eq!(queue.len(), 2);
    assert!(queue.iter().all(|q| q.kind == "full" && q.paths_json.is_some()));

    // 2. Revue reports reviewed coverage for src/a.rs -> coverage jumps to 50% with by='revue'
    let resp = router
        .oneshot(
            Request::post("/api/v1/coverage/kleinbem/demo/revue")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"commit_sha":"revue123","paths":[{"path":"src/a.rs","blob_sha":"blob_a"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    assert_eq!(body["reviewed_files"], 1);
    assert_eq!(body["percentage"], 50.0);

    let files = db.list_file_coverage("kleinbem/demo").await.unwrap();
    let src_a = files.iter().find(|f| f.path == "src/a.rs").unwrap();
    assert_eq!(src_a.by.as_deref(), Some("revue"));
    assert_eq!(src_a.unreviewed_lines, 0);
}
