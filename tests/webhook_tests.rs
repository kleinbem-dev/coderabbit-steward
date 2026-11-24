use axum::body::Body;
use axum::http::{Request, StatusCode};
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::db::Db;
use coderabbit_steward::ingest::webhook::verify_signature;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tower::ServiceExt;

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[test]
fn hmac_verification_accepts_valid_and_rejects_bad() {
    let body = br#"{"zen":"Keep it logically awesome."}"#;
    let sig = sign("super-secret", body);
    assert!(verify_signature("super-secret", &sig, body));
    assert!(!verify_signature("wrong-secret", &sig, body));
    assert!(!verify_signature("super-secret", "sha256=deadbeef", body));
    assert!(!verify_signature("super-secret", "invalid", body));
}

#[tokio::test]
async fn webhook_fails_closed_without_secret_and_rejects_bad_sig() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("wh1.db").display());
    let db = Db::connect(&db_url).await.unwrap();

    // No webhook secret configured -> 401
    let router_no_secret = build_router(
        AppState {
            db: db.clone(),
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: None,
            ntfy: None,
        },
        None,
    );
    let resp = router_no_secret
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "ping")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Bad signature -> 401
    let router = build_router(
        AppState {
            db,
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: Some("whsec".to_string()),
            ntfy: None,
        },
        None,
    );
    let resp = router
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "ping")
                .header("X-Hub-Signature-256", "sha256=0000")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_enqueues_rate_limited_pr_and_completes_on_review_or_merge() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("wh2.db").display());
    let db = Db::connect(&db_url).await.unwrap();
    db.upsert_repo("kleinbem", "nix-config", "public", 1, "main")
        .await
        .unwrap();

    let secret = "test-webhook-secret";
    let router = build_router(
        AppState {
            db: db.clone(),
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: Some(secret.to_string()),
            ntfy: None,
        },
        None,
    );

    // 1. CodeRabbit check_run rate-limited -> auto-enqueues PR #99
    let rl_body = serde_json::json!({
        "action": "completed",
        "repository": { "full_name": "kleinbem/nix-config" },
        "check_run": {
            "id": 1001,
            "name": "CodeRabbit",
            "status": "completed",
            "conclusion": "neutral",
            "started_at": "2026-11-24T01:00:00Z",
            "completed_at": "2026-11-24T01:00:05Z",
            "output": { "title": "Review rate limited" },
            "app": { "slug": "coderabbitai" },
            "pull_requests": [{ "number": 99 }]
        }
    })
    .to_string();

    let resp = router
        .clone()
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "check_run")
                .header("X-Hub-Signature-256", sign(secret, rl_body.as_bytes()))
                .body(Body::from(rl_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let active = db.list_active_queue("kleinbem/nix-config").await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].pr_number, Some(99));
    assert_eq!(active[0].state, "waiting");

    // 2. CodeRabbit check_run finished -> marks PR #99 done
    let done_body = serde_json::json!({
        "action": "completed",
        "repository": { "full_name": "kleinbem/nix-config" },
        "check_run": {
            "id": 1002,
            "name": "CodeRabbit",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-11-24T02:10:00Z",
            "completed_at": "2026-11-24T02:11:00Z",
            "output": { "title": "Finished" },
            "app": { "slug": "coderabbitai" },
            "pull_requests": [{ "number": 99 }]
        }
    })
    .to_string();

    let resp = router
        .clone()
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "check_run")
                .header("X-Hub-Signature-256", sign(secret, done_body.as_bytes()))
                .body(Body::from(done_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let active = db.list_active_queue("kleinbem/nix-config").await.unwrap();
    assert!(active.is_empty());

    // 3. Enqueue PR #100, then merge it without review -> state = merged-unreviewed
    db.enqueue_pr_if_absent("kleinbem/nix-config", 100, None)
        .await
        .unwrap();
    let merge_body = serde_json::json!({
        "action": "closed",
        "repository": { "full_name": "kleinbem/nix-config" },
        "pull_request": {
            "number": 100,
            "merged": true
        }
    })
    .to_string();

    let resp = router
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "pull_request")
                .header("X-Hub-Signature-256", sign(secret, merge_body.as_bytes()))
                .body(Body::from(merge_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let all_q = db.list_queue(Some("kleinbem/nix-config")).await.unwrap();
    let pr100 = all_q.iter().find(|q| q.pr_number == Some(100)).unwrap();
    assert_eq!(pr100.state, "merged-unreviewed");
}

#[tokio::test]
async fn webhook_ignores_rerequested_and_requeues_rate_limited_triggered_pr() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("wh3.db").display());
    let db = Db::connect(&db_url).await.unwrap();
    db.upsert_repo("kleinbem", "nix-config", "public", 1, "main")
        .await
        .unwrap();

    let row = db
        .enqueue_pr_if_absent("kleinbem/nix-config", 101, None)
        .await
        .unwrap()
        .unwrap();
    assert!(db.mark_queue_triggered_if_waiting(row.id).await.unwrap());

    let secret = "test-webhook-secret";
    let router = build_router(
        AppState {
            db: db.clone(),
            api_token: None,
            trust_authentik_header: false,
            webhook_secret: Some(secret.to_string()),
            ntfy: None,
        },
        None,
    );

    // 1. rerequested check_run must NOT complete the active queue item
    let rereq_body = serde_json::json!({
        "action": "rerequested",
        "repository": { "full_name": "kleinbem/nix-config" },
        "check_run": {
            "id": 2001,
            "name": "CodeRabbit",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-11-24T01:00:00Z",
            "completed_at": "2026-11-24T01:01:00Z",
            "output": { "title": "Finished" },
            "app": { "slug": "coderabbitai" },
            "pull_requests": [{ "number": 101 }]
        }
    })
    .to_string();

    let resp = router
        .clone()
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "check_run")
                .header("X-Hub-Signature-256", sign(secret, rereq_body.as_bytes()))
                .body(Body::from(rereq_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let active = db.list_active_queue("kleinbem/nix-config").await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].state, "triggered");

    // 2. Rate-limited check_run returns the triggered row back to waiting
    let rl_body = serde_json::json!({
        "action": "completed",
        "repository": { "full_name": "kleinbem/nix-config" },
        "check_run": {
            "id": 2002,
            "name": "CodeRabbit",
            "status": "completed",
            "conclusion": "neutral",
            "started_at": "2026-11-24T02:00:00Z",
            "completed_at": "2026-11-24T02:00:05Z",
            "output": { "title": "Review rate limited" },
            "app": { "slug": "coderabbitai" },
            "pull_requests": [{ "number": 101 }]
        }
    })
    .to_string();

    let resp = router
        .oneshot(
            Request::post("/webhook")
                .header("X-GitHub-Event", "check_run")
                .header("X-Hub-Signature-256", sign(secret, rl_body.as_bytes()))
                .body(Body::from(rl_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let active = db.list_active_queue("kleinbem/nix-config").await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].state, "waiting");
}
