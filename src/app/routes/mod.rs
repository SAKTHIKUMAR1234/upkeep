mod activity;
mod assets;
mod events;
mod guard;
mod operations;
mod runs;
mod servers;
mod session;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::json;
use tokio::sync::Semaphore;

use super::auth::session::{SessionState, has_csrf, read_cookie, state};
use super::operations::OperationError;
use super::runs::ssh::SshError;
use super::runs::{Executor, RunError};
use super::store::{Activity, Db, Server, StoreError};
use crate::time::now_millis;

/// What every handler shares.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub executor: Arc<Executor>,
    /// The port the service listens on, for the host and origin guard.
    pub port: u16,
    /// Limits parallel password checks: each one takes 64 MiB.
    pub hashing: Arc<Semaphore>,
}

impl AppState {
    pub fn new(db: Arc<Db>, executor: Arc<Executor>, port: u16) -> Self {
        Self {
            db,
            executor,
            port,
            hashing: Arc::new(Semaphore::new(2)),
        }
    }

    /// Writes an activity line. A failed write is logged, not shown to the user.
    pub fn record(&self, user: &str, action: Activity, server: Option<&str>, detail: &str) {
        if let Err(error) = self.db.record(now_millis(), user, action, server, detail) {
            tracing::error!(%error, "cannot write the activity log");
        }
    }

    pub fn server(&self, id: i64) -> Result<Server, AppError> {
        self.db.server(id)?.ok_or(AppError::NotFound("server"))
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route(
            "/session",
            get(session::current)
                .post(session::sign_in)
                .delete(session::sign_out),
        )
        .route("/overview", get(servers::overview))
        .route("/ssh-keys", get(servers::ssh_keys))
        .route("/servers", get(servers::list).post(servers::add))
        .route("/servers/test", post(servers::test))
        .route(
            "/servers/{id}",
            get(servers::show)
                .put(servers::edit)
                .delete(servers::delete),
        )
        .route(
            "/servers/{id}/operations",
            get(operations::list).post(operations::create),
        )
        .route(
            "/servers/{id}/operations/{folder}",
            get(operations::show)
                .put(operations::edit)
                .delete(operations::delete),
        )
        .route("/servers/{id}/operations/{folder}/runs", post(runs::start))
        .route("/runs", get(runs::list))
        .route("/runs/export.csv", get(runs::export))
        .route("/runs/{id}", get(runs::show))
        .route("/runs/{id}/output", get(runs::output))
        .route("/runs/{id}/events", get(events::run_events))
        .route("/runs/{id}/stop", post(runs::stop))
        .route("/events", get(events::changes))
        .route("/filters", get(activity::filters))
        .route("/activity", get(activity::list))
        .fallback(|| async { AppError::NotFound("route") });
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/{file}", get(assets::file))
        .nest("/api", api)
        .layer(middleware::from_fn_with_state(state.clone(), guard::guard))
        .with_state(state)
}

/// A signed-in user, checked on every request: a live session, and the CSRF header on every
/// request that changes something.
pub struct Auth {
    pub email: String,
    pub token: String,
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, AppError> {
        let token = read_cookie(&parts.headers).ok_or(AppError::SignedOut)?;
        let session = app.db.session(token)?.ok_or(AppError::SignedOut)?;
        let now = now_millis();
        match state(&session, now) {
            SessionState::Live => {}
            ended => {
                app.db.delete_session(token)?;
                return Err(match ended {
                    SessionState::Idle => AppError::Locked,
                    _ => AppError::SignedOut,
                });
            }
        }
        if !matches!(parts.method, Method::GET | Method::HEAD)
            && !has_csrf(&parts.headers, &session)
        {
            return Err(AppError::Csrf);
        }
        app.db.touch_session(token, now)?;
        Ok(Self {
            email: session.email,
            token: token.to_owned(),
        })
    }
}

/// An API error as `{"error": "...", "code": "..."}` with its status.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Sign in first.")]
    SignedOut,
    #[error("The app locked after 15 minutes without use. Enter your password.")]
    Locked,
    #[error("Missing or wrong X-CSRF-Token header.")]
    Csrf,
    #[error("This request comes from another site.")]
    ForeignOrigin,
    #[error("Wrong email or password.")]
    BadSignIn,
    #[error("Too many failed sign-ins. Try again in {}.", wait_text(*.0))]
    TooManyAttempts(i64),
    #[error("{0}")]
    Invalid(String),
    #[error("No such {0}.")]
    NotFound(&'static str),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Operation(#[from] OperationError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.into())
    }
}

fn wait_text(milliseconds: i64) -> String {
    let seconds = (milliseconds + 999) / 1000;
    if seconds < 120 {
        format!("{seconds} s")
    } else {
        format!("{} min", (seconds + 59) / 60)
    }
}

impl AppError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::SignedOut => (StatusCode::UNAUTHORIZED, "signed_out"),
            Self::Locked => (StatusCode::UNAUTHORIZED, "locked"),
            Self::BadSignIn => (StatusCode::UNAUTHORIZED, "bad_sign_in"),
            Self::Csrf | Self::ForeignOrigin => (StatusCode::FORBIDDEN, "forbidden"),
            Self::TooManyAttempts(_) => (StatusCode::TOO_MANY_REQUESTS, "too_many_attempts"),
            Self::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            Self::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::Operation(error) | Self::Run(RunError::Operation(error)) => {
                operation_status(error)
            }
            Self::Run(RunError::Busy { .. } | RunError::NotActive(_)) => {
                (StatusCode::CONFLICT, "conflict")
            }
            Self::Run(_) | Self::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

fn operation_status(error: &OperationError) -> (StatusCode, &'static str) {
    match error {
        OperationError::InvalidFolder | OperationError::NoShebang(_) => {
            (StatusCode::BAD_REQUEST, "invalid")
        }
        OperationError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        OperationError::Exists => (StatusCode::CONFLICT, "conflict"),
        OperationError::Ssh(SshError::KeyOpen(_) | SshError::KeyRead { .. }) => {
            (StatusCode::BAD_REQUEST, "key")
        }
        _ => (StatusCode::BAD_GATEWAY, "server"),
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
            "Internal error. See the service log.".to_owned()
        } else {
            self.to_string()
        };
        (status, Json(json!({"error": message, "code": code}))).into_response()
    }
}
