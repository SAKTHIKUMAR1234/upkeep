use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::runners::{Runner, RunnerError};
use super::session::Session;
use super::users::{Role, checked_email};
use super::{WebError, WebState};
use crate::adapter::manifest::is_valid_name;

const AUDIT_LIMIT: u32 = 200;
const JOBS_LIMIT: u32 = 200;

/// The `/api` routes. `routes::require_session` puts the `Session` in front of each.
pub fn router() -> Router<WebState> {
    Router::new()
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/hosts", get(hosts))
        .route("/hosts/{host}/plans", get(plans))
        .route("/hosts/{host}/run", post(run))
        .route("/hosts/{host}/jobs", get(jobs))
        .route("/hosts/{host}/jobs/{id}", get(job))
        .route("/hosts/{host}/jobs/{id}/log", get(job_log))
        .route("/hosts/{host}/jobs/{id}/cancel", post(cancel))
        .route("/users", get(users).post(set_user).delete(remove_user))
        .route("/audit", get(audit))
}

type ApiResult = Result<Json<Value>, WebError>;

fn runner<'a>(state: &'a WebState, host: &str) -> Result<&'a Runner, WebError> {
    state
        .runners
        .get(host)
        .ok_or_else(|| WebError::UnknownHost(host.to_owned()))
}

async fn me(Extension(session): Extension<Session>) -> Json<Session> {
    Json(session)
}

async fn logout(State(state): State<WebState>, headers: HeaderMap) -> Result<Response, WebError> {
    if let Some(id) = state.session_cookie.read(&headers) {
        state.db.delete_session(id)?;
    }
    let cookie = [(header::SET_COOKIE, state.session_cookie.clear())];
    Ok((cookie, Json(json!({"signed_out": true}))).into_response())
}

async fn hosts(State(state): State<WebState>) -> Json<Value> {
    Json(json!({"hosts": state.runners.cards().await}))
}

async fn plans(State(state): State<WebState>, Path(host): Path<String>) -> ApiResult {
    Ok(Json(runner(&state, &host)?.get("/plans").await?))
}

#[derive(Deserialize)]
struct RunBody {
    plan: String,
}

/// Starts a plan, confirmed in the page's dialog. Single `adapter:action` targets are not offered.
async fn run(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path(host): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Response, WebError> {
    session.require(Role::Operator)?;
    let runner = runner(&state, &host)?;
    if !is_valid_name(&body.plan) {
        return Err(WebError::Invalid(format!("invalid plan {:?}", body.plan)));
    }
    let request = json!({"target": body.plan, "confirm": true, "requested_by": session.email});
    let started = runner.post("/run", &request).await;
    let detail = match &started {
        Ok(job) => format!("{}: job {}", body.plan, job["id"]),
        Err(error) => format!("{}: refused: {error}", body.plan),
    };
    state
        .db
        .record(&session.email, "run", Some(&host), &detail)?;
    match started {
        Ok(job) => Ok((StatusCode::CREATED, Json(job)).into_response()),
        Err(RunnerError::Status { status: 409, .. }) => Err(WebError::Busy),
        Err(error) => Err(error.into()),
    }
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<u32>,
}

async fn jobs(
    State(state): State<WebState>,
    Path(host): Path<String>,
    Query(query): Query<Limit>,
) -> ApiResult {
    let limit = query.limit.unwrap_or(20).clamp(1, JOBS_LIMIT);
    Ok(Json(
        runner(&state, &host)?
            .get(&format!("/jobs?limit={limit}"))
            .await?,
    ))
}

async fn job(State(state): State<WebState>, Path((host, id)): Path<(String, i64)>) -> ApiResult {
    Ok(Json(
        runner(&state, &host)?.get(&format!("/jobs/{id}")).await?,
    ))
}

async fn job_log(
    State(state): State<WebState>,
    Path((host, id)): Path<(String, i64)>,
) -> Result<Response, WebError> {
    let log = runner(&state, &host)?.log(id).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], log).into_response())
}

async fn cancel(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Path((host, id)): Path<(String, i64)>,
) -> ApiResult {
    session.require(Role::Operator)?;
    let cancelled = runner(&state, &host)?
        .post(&format!("/jobs/{id}/cancel"), &json!({}))
        .await;
    let detail = match &cancelled {
        Ok(_) => format!("job {id}"),
        Err(error) => format!("job {id}: refused: {error}"),
    };
    state
        .db
        .record(&session.email, "cancel", Some(&host), &detail)?;
    Ok(Json(cancelled?))
}

async fn users(State(state): State<WebState>, Extension(session): Extension<Session>) -> ApiResult {
    session.require(Role::Admin)?;
    Ok(Json(json!({"users": state.db.users()?})))
}

#[derive(Deserialize)]
struct UserBody {
    email: String,
    role: Option<Role>,
}

impl UserBody {
    /// The checked email. Admins do not change their own entry, so the last admin cannot lock everyone out.
    fn email(&self, session: &Session) -> Result<String, WebError> {
        let email = checked_email(&self.email)
            .ok_or_else(|| WebError::Invalid(format!("invalid email {:?}", self.email)))?;
        if email == session.email {
            return Err(WebError::Invalid("you cannot change your own entry".into()));
        }
        Ok(email)
    }
}

async fn set_user(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Json(body): Json<UserBody>,
) -> ApiResult {
    session.require(Role::Admin)?;
    let email = body.email(&session)?;
    let role = body
        .role
        .ok_or_else(|| WebError::Invalid("choose a role".into()))?;
    state.db.set_user(&email, role, &session.email)?;
    let detail = format!("{email} as {}", role.as_str());
    state.db.record(&session.email, "user add", None, &detail)?;
    Ok(Json(json!({"email": email, "role": role})))
}

async fn remove_user(
    State(state): State<WebState>,
    Extension(session): Extension<Session>,
    Json(body): Json<UserBody>,
) -> ApiResult {
    session.require(Role::Admin)?;
    let email = body.email(&session)?;
    if !state.db.remove_user(&email)? {
        return Err(WebError::NotFound(format!("user {email}")));
    }
    state
        .db
        .record(&session.email, "user remove", None, &email)?;
    Ok(Json(json!({"removed": email})))
}

async fn audit(State(state): State<WebState>, Extension(session): Extension<Session>) -> ApiResult {
    session.require(Role::Admin)?;
    Ok(Json(json!({"audit": state.db.audit(AUDIT_LIMIT)?})))
}
