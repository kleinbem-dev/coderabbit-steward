use axum::body::Body;
use axum::http::{Request, StatusCode};
use coderabbit_steward::api::{AppState, build_router};
use coderabbit_steward::db::Db;
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn health_budget_queue_and_auth() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db_url = format!("sqlite://{}?mode=rwc", db_path.display());
    let db = Db::connect(&db_url).await.unwrap();
    db.upsert_repo("kleinbem", "nix-config", "public", 1, "main")
        .await
        .unwrap();

    let router = build_router(
        AppState {
            db,
            api_token: Some("secret-token".to_string()),
        },
        None,
    );

    // Health is unauthenticated
    let resp = router
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Protected endpoint rejects missing token
    let resp = router
        .clone()
        .oneshot(Request::get("/api/v1/budget").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Protected endpoint accepts valid bearer token
    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/v1/budget")
                .header("Authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let items: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["repo"], "kleinbem/nix-config");
    assert_eq!(items[0]["free"], true);

    // Queue POST + GET
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/reviews")
                .header("Authorization", "Bearer secret-token")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"repo":"kleinbem/nix-config","pr":42}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = router
        .oneshot(
            Request::get("/api/v1/queue")
                .header("Authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let q: Vec<Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0]["pr_number"], 42);
}
