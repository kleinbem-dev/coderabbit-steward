use crate::budget::BudgetSnapshot;
use crate::config::Mode;
use crate::db::{Db, FindingRow, QueueRow, SpendRow};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use subtle::ConstantTimeEq;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub api_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FleetRepoBudget {
    pub repo: String,
    pub owner: String,
    pub name: String,
    pub stars: i64,
    pub free: bool,
    pub remaining: u32,
    pub limit: u32,
    pub refill_minutes: u32,
    pub next_free_at: Option<String>,
    pub confidence: String,
    pub queue_length: usize,
    pub coverage_pct: f64,
    pub open_findings: usize,
}

#[derive(Debug, Serialize)]
pub struct RepoDetailResponse {
    pub repo: String,
    pub stars: i64,
    pub budget: BudgetSnapshot,
    pub spends: Vec<SpendRow>,
    pub queue: Vec<QueueRow>,
    pub findings: Vec<FindingRow>,
}

#[derive(Debug, Deserialize)]
pub struct QueueQuery {
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindingsQuery {
    pub repo: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateReviewRequest {
    pub repo: String,
    pub pr: Option<i64>,
    pub paths: Option<Vec<String>>,
    pub full: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct PatchFindingRequest {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct PatchSettingsRequest {
    pub mode: Option<String>,
}

pub fn build_router(state: AppState, web_dir: Option<PathBuf>) -> Router {
    let protected = Router::new()
        .route("/api/v1/budget", get(get_fleet_budget))
        .route("/api/v1/budget/{owner}/{repo}", get(get_repo_budget))
        .route("/api/v1/repo/{owner}/{repo}", get(get_repo_detail))
        .route("/api/v1/reviews", post(post_review_request))
        .route("/api/v1/queue", get(get_queue))
        .route("/api/v1/coverage/{owner}/{repo}", get(get_coverage))
        .route("/api/v1/findings", get(get_findings))
        .route("/api/v1/findings/{id}", patch(patch_finding))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let mut router = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .merge(protected)
        .layer(CorsLayer::permissive())
        .with_state(state);

    if let Some(dir) = web_dir {
        let index = if dir.join("index.html").is_file() {
            dir.join("index.html")
        } else {
            dir.join("200.html")
        };
        let service = ServeDir::new(&dir).fallback(ServeFile::new(index));
        router = router.fallback_service(service);
    }

    router
}

async fn require_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_deref() else {
        return next.run(req).await;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let authorized = provided
        .is_some_and(|tok| tok.as_bytes().ct_eq(expected.as_bytes()).into());

    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response()
    }
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy",
        "service": "coderabbit-steward"
    }))
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let now = Timestamp::now();
    let repos = state.db.list_repos().await.unwrap_or_default();
    let mut free_count = 0usize;
    for r in &repos {
        let full = format!("{}/{}", r.owner, r.name);
        if let Ok(ledger) = state.db.load_budget_ledger(&full).await
            && ledger.snapshot(now).free
        {
            free_count += 1;
        }
    }
    let body = format!(
        "# HELP cr_steward_repos_total Tracked repositories\n\
         # TYPE cr_steward_repos_total gauge\n\
         cr_steward_repos_total {}\n\
         # HELP cr_steward_repos_free Repositories with free review budget\n\
         # TYPE cr_steward_repos_free gauge\n\
         cr_steward_repos_free {}\n",
        repos.len(),
        free_count
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

async fn get_fleet_budget(
    State(state): State<AppState>,
) -> Result<Json<Vec<FleetRepoBudget>>, (StatusCode, String)> {
    let now = Timestamp::now();
    let repos = state
        .db
        .list_repos()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let queue = state
        .db
        .list_queue(None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let findings = state
        .db
        .list_findings(None, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        let full = format!("{}/{}", r.owner, r.name);
        let ledger = state
            .db
            .load_budget_ledger(&full)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let snap = ledger.snapshot(now);
        let q_len = queue
            .iter()
            .filter(|q| q.repo == full && (q.state == "waiting" || q.state == "triggered"))
            .count();
        let open_f = findings
            .iter()
            .filter(|f| f.repo == full && (f.status == "new" || f.status == "real" || f.status == "needs-martin"))
            .count();

        out.push(FleetRepoBudget {
            repo: full,
            owner: r.owner,
            name: r.name,
            stars: r.stars,
            free: snap.free,
            remaining: snap.remaining,
            limit: snap.limit,
            refill_minutes: snap.refill_minutes,
            next_free_at: snap.next_free_at.map(|t| t.to_string()),
            confidence: snap.confidence.as_str().to_string(),
            queue_length: q_len,
            coverage_pct: 0.0,
            open_findings: open_f,
        });
    }

    out.sort_by(|a, b| b.free.cmp(&a.free).then_with(|| a.repo.cmp(&b.repo)));
    Ok(Json(out))
}

async fn get_repo_budget(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<BudgetSnapshot>, (StatusCode, String)> {
    let full = format!("{owner}/{repo}");
    let ledger = state
        .db
        .load_budget_ledger(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ledger.snapshot(Timestamp::now())))
}

async fn get_repo_detail(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<RepoDetailResponse>, (StatusCode, String)> {
    let full = format!("{owner}/{repo}");
    let repos = state
        .db
        .list_repos()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let stars = repos
        .into_iter()
        .find(|r| r.owner == owner && r.name == repo)
        .map(|r| r.stars)
        .unwrap_or(0);
    let ledger = state
        .db
        .load_budget_ledger(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let spends = state
        .db
        .list_spends(&full, 20)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let queue = state
        .db
        .list_queue(Some(&full))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let findings = state
        .db
        .list_findings(Some(&full), None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(RepoDetailResponse {
        repo: full,
        stars,
        budget: ledger.snapshot(Timestamp::now()),
        spends,
        queue,
        findings,
    }))
}

async fn post_review_request(
    State(state): State<AppState>,
    Json(req): Json<CreateReviewRequest>,
) -> Result<(StatusCode, Json<QueueRow>), (StatusCode, String)> {
    let (kind, paths_json) = if req.full.unwrap_or(false) {
        ("full", None)
    } else if let Some(paths) = req.paths {
        ("proactive", Some(serde_json::to_string(&paths).unwrap_or_default()))
    } else {
        ("on-demand", None)
    };
    let row = state
        .db
        .enqueue_review(&req.repo, kind, req.pr, paths_json.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn get_queue(
    State(state): State<AppState>,
    Query(q): Query<QueueQuery>,
) -> Result<Json<Vec<QueueRow>>, (StatusCode, String)> {
    let rows = state
        .db
        .list_queue(q.repo.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows))
}

async fn get_coverage(
    Path((owner, repo)): Path<(String, String)>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "repo": format!("{owner}/{repo}"),
        "percentage": 0.0,
        "unreviewed_files": []
    }))
}

async fn get_findings(
    State(state): State<AppState>,
    Query(q): Query<FindingsQuery>,
) -> Result<Json<Vec<FindingRow>>, (StatusCode, String)> {
    let rows = state
        .db
        .list_findings(q.repo.as_deref(), q.status.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows))
}

async fn patch_finding(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<PatchFindingRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let updated = state
        .db
        .update_finding_status(id, &req.status, "api")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "finding not found".to_string()))
    }
}

async fn get_settings(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let s = state
        .db
        .get_settings()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!(s)))
}

async fn patch_settings(
    State(state): State<AppState>,
    Json(req): Json<PatchSettingsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(m) = req.mode {
        let mode = Mode::parse(&m).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        state
            .db
            .set_mode(mode)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let s = state
        .db
        .get_settings()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!(s)))
}
