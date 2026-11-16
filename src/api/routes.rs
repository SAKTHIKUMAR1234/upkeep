use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::{Token, require_token};
use super::log_stream;
use crate::adapter::health;
use crate::engine::{RunError, RunRequest, Runner};
use crate::store::StoreError;

#[derive(Clone)]
struct ApiState {
    runner: Arc<Runner>,
    requester: &'static str,
}

/// Routes for the local unix socket. File permissions on the socket guard access.
pub fn local_router(runner: Arc<Runner>) -> Router {
    router(ApiState {
        runner,
        requester: "local",
    })
}

/// Routes for the TCP listener, behind the bearer token.
pub fn remote_router(runner: Arc<Runner>, token: Token) -> Router {
    let state = ApiState {
        runner,
        requester: "api",
    };
    router(state).layer(middleware::from_fn_with_state(token, require_token))
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/adapters", get(adapters))
        .route("/run", post(run))
        .route("/jobs", get(jobs))
        .route("/jobs/{id}", get(job))
        .route("/jobs/{id}/log", get(job_log))
        .route("/jobs/{id}/cancel", post(cancel))
        .with_state(state)
}

struct ApiError(RunError);

impl From<RunError> for ApiError {
    fn from(error: RunError) -> Self {
        Self(error)
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self(RunError::Store(error))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            RunError::Plan(_) => StatusCode::UNPROCESSABLE_ENTITY,
            RunError::ConfirmRequired(_) => StatusCode::PRECONDITION_REQUIRED,
            RunError::Store(StoreError::Busy) | RunError::NotActive(_) => StatusCode::CONFLICT,
            RunError::Store(StoreError::NotFound(_)) => StatusCode::NOT_FOUND,
            RunError::Store(_) | RunError::Secrets(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

async fn health_check(State(state): State<ApiState>) -> Json<health::HealthReport> {
    let catalog = state.runner.catalog();
    Json(health::check_all(&catalog, state.runner.paths()).await)
}

async fn adapters(State(state): State<ApiState>) -> Json<Value> {
    let catalog = state.runner.catalog();
    let adapters: Vec<_> = catalog.adapters.values().collect();
    Json(json!({"adapters": adapters, "errors": catalog.errors}))
}

async fn run(
    State(state): State<ApiState>,
    Json(request): Json<RunRequest>,
) -> Result<Response, ApiError> {
    let id = state.runner.start(request, state.requester)?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))).into_response())
}

#[derive(Deserialize)]
struct JobsQuery {
    limit: Option<u32>,
}

async fn jobs(
    State(state): State<ApiState>,
    Query(query): Query<JobsQuery>,
) -> Result<Json<Value>, ApiError> {
    let jobs = state.runner.store().jobs(query.limit.unwrap_or(50))?;
    Ok(Json(json!({"jobs": jobs})))
}

async fn job(State(state): State<ApiState>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let store = state.runner.store();
    let job = store.job(id)?;
    Ok(Json(json!({"job": job, "steps": store.steps(id)?})))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    follow: bool,
}

async fn job_log(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Query(query): Query<LogQuery>,
) -> Result<Response, ApiError> {
    state.runner.store().job(id)?;
    let stream = log_stream::job_log(state.runner.clone(), id, query.follow);
    let headers = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    Ok((headers, Body::from_stream(stream)).into_response())
}

async fn cancel(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    state.runner.cancel(id)?;
    Ok(Json(json!({"id": id, "cancel_requested": true})))
}
