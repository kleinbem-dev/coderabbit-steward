use crate::actuator::ntfy::NtfyClient;
use crate::actuator::revue::RevueClient;
use crate::budget::BudgetSnapshot;
use crate::config::Mode;
use crate::coverage::{self, CoverageSummary};
use crate::db::{Db, FindingRow, QueueRow, SpendRow};
use crate::ingest::webhook::{handle_webhook_event, verify_signature};
use crate::synthetic::split_full_repo_batches;
use axum::body::Bytes;
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
    pub trust_authentik_header: bool,
    pub webhook_secret: Option<String>,
    pub ntfy: Option<NtfyClient>,
    pub revue: Option<RevueClient>,
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
    pub proactive_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct RepoDetailResponse {
    pub repo: String,
    pub stars: i64,
    pub proactive_enabled: bool,
    pub budget: BudgetSnapshot,
    pub coverage: CoverageSummary,
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
    pub repo_daily_cap: Option<i64>,
    pub fleet_daily_cap: Option<i64>,
    pub quiet_minutes: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRepoRequest {
    pub proactive_enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct RevueCoverageFile {
    pub path: String,
    pub blob_sha: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevueCoverageReport {
    pub commit_sha: String,
    pub paths: Vec<RevueCoverageFile>,
}

pub fn build_router(state: AppState, web_dir: Option<PathBuf>) -> Router {
    let protected = Router::new()
        .route("/api/v1/budget", get(get_fleet_budget))
        .route("/api/v1/budget/{owner}/{repo}", get(get_repo_budget))
        .route(
            "/api/v1/repo/{owner}/{repo}",
            get(get_repo_detail).patch(patch_repo),
        )
        .route(
            "/api/v1/repo/{owner}/{repo}/queue",
            axum::routing::delete(clear_repo_queue),
        )
        .route("/api/v1/reviews", post(post_review_request))
        .route("/api/v1/queue", get(get_queue))
        .route(
            "/api/v1/queue/{id}",
            axum::routing::delete(delete_queue_item),
        )
        .route("/api/v1/coverage/{owner}/{repo}", get(get_coverage))
        .route(
            "/api/v1/coverage/{owner}/{repo}/revue",
            post(post_revue_coverage),
        )
        .route("/api/v1/findings", get(get_findings))
        .route("/api/v1/findings/{id}", patch(patch_finding))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let mut router = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/webhook", post(post_webhook))
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

    let bearer_ok = provided
        .is_some_and(|tok| tok.as_bytes().ct_eq(expected.as_bytes()).into());
    let authentik_ok = state.trust_authentik_header
        && headers
            .get("x-authentik-username")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.trim().is_empty());

    if bearer_ok || authentik_ok {
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

async fn post_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(secret) = state.webhook_secret.as_deref().filter(|s| !s.is_empty()) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "webhook secret not configured" })),
        ));
    };

    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(secret, sig, &body) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid webhook signature" })),
        ));
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let outcome = handle_webhook_event(
        &state.db,
        state.ntfy.as_ref(),
        state.revue.as_ref(),
        event,
        &body,
    )
    .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "ignored": outcome.ignored,
        "message": outcome.message,
    })))
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
        let cov_files = state
            .db
            .list_file_coverage(&full)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cov_pct = coverage::summarize(&full, &cov_files, now).percentage;

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
            coverage_pct: cov_pct,
            open_findings: open_f,
            proactive_enabled: r.proactive_enabled != 0,
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
    let repo_row = repos.into_iter().find(|r| r.owner == owner && r.name == repo);
    let stars = repo_row.as_ref().map_or(0, |r| r.stars);
    let proactive_enabled = repo_row.as_ref().is_none_or(|r| r.proactive_enabled != 0);
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
    let cov_files = state
        .db
        .list_file_coverage(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let now = Timestamp::now();

    Ok(Json(RepoDetailResponse {
        repo: full.clone(),
        stars,
        proactive_enabled,
        budget: ledger.snapshot(now),
        coverage: coverage::summarize(&full, &cov_files, now),
        spends,
        queue,
        findings,
    }))
}

async fn post_review_request(
    State(state): State<AppState>,
    Json(req): Json<CreateReviewRequest>,
) -> Result<(StatusCode, Json<QueueRow>), (StatusCode, String)> {
    if req.full.unwrap_or(false) {
        let cov_files = state
            .db
            .list_file_coverage(&req.repo)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let file_cap = state
            .db
            .list_repos()
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            .find(|r| format!("{}/{}", r.owner, r.name) == req.repo)
            .map(|r| usize::try_from(r.file_cap.max(1)).unwrap_or(50))
            .unwrap_or(50);

        let batches = split_full_repo_batches(&cov_files, file_cap);
        if batches.is_empty() {
            let row = state
                .db
                .enqueue_review(&req.repo, "full", None, None)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            return Ok((StatusCode::CREATED, Json(row)));
        }
        let batches_json: Vec<String> = batches
            .into_iter()
            .map(|b| {
                let paths: Vec<String> = b.files.into_iter().map(|f| f.path).collect();
                serde_json::to_string(&paths).unwrap_or_default()
            })
            .collect();
        let row = state
            .db
            .enqueue_review_batches(&req.repo, "full", &batches_json)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Ok((StatusCode::CREATED, Json(row)));
    }

    let (kind, paths_json) = if let Some(paths) = req.paths {
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
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<CoverageSummary>, (StatusCode, String)> {
    let full = format!("{owner}/{repo}");
    let files = state
        .db
        .list_file_coverage(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(coverage::summarize(&full, &files, Timestamp::now())))
}

async fn post_revue_coverage(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(report): Json<RevueCoverageReport>,
) -> Result<Json<CoverageSummary>, (StatusCode, String)> {
    let full = format!("{owner}/{repo}");
    let reported: Vec<(String, Option<String>)> = report
        .paths
        .into_iter()
        .map(|p| (p.path, p.blob_sha))
        .collect();
    let updated = state
        .db
        .apply_revue_coverage(&full, &report.commit_sha, &reported)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(coverage::summarize(&full, &updated, Timestamp::now())))
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

async fn delete_queue_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let deleted = state
        .db
        .delete_queue_item(id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "queue item not found".to_string()))
    }
}

async fn clear_repo_queue(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let full = format!("{owner}/{repo}");
    let deleted = state
        .db
        .clear_repo_waiting_queue(&full)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "repo": full,
        "deleted": deleted,
    })))
}

async fn patch_repo(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(req): Json<PatchRepoRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let updated = state
        .db
        .set_repo_proactive(&owner, &repo, req.proactive_enabled)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !updated {
        return Err((StatusCode::NOT_FOUND, "repository not found".to_string()));
    }
    Ok(Json(serde_json::json!({
        "repo": format!("{owner}/{repo}"),
        "proactive_enabled": req.proactive_enabled,
    })))
}

async fn patch_settings(
    State(state): State<AppState>,
    Json(req): Json<PatchSettingsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mode = match req.mode {
        Some(m) => Some(Mode::parse(&m).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?),
        None => None,
    };
    if let Some(cap) = req.repo_daily_cap
        && !(0..=500).contains(&cap)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "repo_daily_cap must be between 0 and 500".to_string(),
        ));
    }
    if let Some(cap) = req.fleet_daily_cap
        && !(0..=500).contains(&cap)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "fleet_daily_cap must be between 0 and 500".to_string(),
        ));
    }
    if let Some(qm) = req.quiet_minutes
        && !(0..=1440).contains(&qm)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "quiet_minutes must be between 0 and 1440".to_string(),
        ));
    }
    let s = state
        .db
        .update_settings(
            mode,
            req.repo_daily_cap,
            req.fleet_daily_cap,
            req.quiet_minutes,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!(s)))
}
