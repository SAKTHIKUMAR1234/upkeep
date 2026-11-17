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
#[cfg(test)]
mod website_tests;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde_json::json;
use tokio::sync::Semaphore;

use super::auth::session::{Cookies, SessionState, has_csrf, read_cookie, state};
use super::auth::throttle::IpThrottle;
use super::operations::OperationError;
use super::runs::ssh::SshError;
use super::runs::{Executor, RunError};
use super::serve::ServeConfig;
use super::store::{Activity, Db, Server, StoreError};
use crate::time::now_millis;

/// What every handler shares.
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub executor: Arc<Executor>,
    /// The port the service listens on, for the host and origin guard.
    pub port: u16,
    pub serve: Arc<ServeConfig>,
    /// Limits parallel password checks: each one takes 64 MiB.
    pub hashing: Arc<Semaphore>,
    /// Failed sign-ins per client address, on a website only: on the loopback address every
    /// client has the same address.
    pub throttle: Option<Arc<IpThrottle>>,
}

impl AppState {
    pub fn new(db: Arc<Db>, executor: Arc<Executor>, port: u16, serve: ServeConfig) -> Self {
        Self {
            db,
            executor,
            port,
            throttle: serve.is_website().then(Arc::default),
            serve: Arc::new(serve),
            hashing: Arc::new(Semaphore::new(2)),
        }
    }

    pub fn cookies(&self) -> Cookies {
        Cookies {
            secure: self.serve.is_website(),
            limits: self.serve.sessions,
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
        .route("/session/ping", post(session::ping))
        .route("/session/lock", post(session::lock))
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
        .fallback(|| async { AppError::NotFound("route") })
        .layer(DefaultBodyLimit::max(MAX_BODY));
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/{file}", get(assets::file))
        .nest("/api", api)
        .layer(middleware::from_fn_with_state(state.clone(), guard::guard))
        .with_state(state)
}

/// Marks a request the page sends by itself, such as a refresh after an event.
pub const BACKGROUND_HEADER: &str = "x-upkeep-background";

/// The largest JSON body the API reads.
const MAX_BODY: usize = 1024 * 1024;

/// A signed-in user, checked on every request: a live session, and the CSRF header on every
/// request that changes something. The request counts as use of the session, unless the page
/// marks it with [`BACKGROUND_HEADER`] because it sent it without user input.
pub struct Auth {
    pub email: String,
    pub token: String,
}

impl Auth {
    fn check(parts: &Parts, app: &AppState, now: i64) -> Result<Self, AppError> {
        let token = read_cookie(&parts.headers).ok_or(AppError::SignedOut)?;
        let session = app.db.session(token)?.ok_or(AppError::SignedOut)?;
        match state(&session, now, app.serve.sessions) {
            SessionState::Live => {}
            // The row stays, so every request of the idle session answers the same until a
            // sign-in removes it.
            SessionState::Idle => return Err(AppError::Locked),
            SessionState::Expired => {
                app.db.delete_session(token)?;
                return Err(AppError::SignedOut);
            }
        }
        if !matches!(parts.method, Method::GET | Method::HEAD)
            && !has_csrf(&parts.headers, &session)
        {
            return Err(AppError::Csrf);
        }
        Ok(Self {
            email: session.email,
            token: token.to_owned(),
        })
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, AppError> {
        let now = now_millis();
        let auth = Self::check(parts, app, now)?;
        if !parts.headers.contains_key(BACKGROUND_HEADER) {
            app.db.touch_session(&auth.token, now)?;
        }
        Ok(auth)
    }
}

/// A signed-in user of an event stream: checked like [`Auth`], but an open stream is not use, so
/// it does not keep the session alive.
pub struct StreamAuth(pub Auth);

impl FromRequestParts<AppState> for StreamAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, AppError> {
        Auth::check(parts, app, now_millis()).map(Self)
    }
}

/// An API error as `{"error": "...", "code": "..."}` with its status.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Sign in first.")]
    SignedOut,
    #[error("Upkeep locked because it was not used. Enter your password.")]
    Locked,
    #[error("Missing or wrong X-CSRF-Token header.")]
    Csrf,
    #[error("This request comes from another site.")]
    ForeignOrigin,
    #[error("Wrong email or password.")]
    BadSignIn,
    #[error("Too many failed sign-ins. Try again in {}.", wait_text(*.0))]
    TooManyAttempts(i64),
    #[error("The request took too long.")]
    Timeout,
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
            Self::Timeout => (StatusCode::SERVICE_UNAVAILABLE, "timeout"),
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
        let mut response = (status, Json(json!({"error": message, "code": code}))).into_response();
        if let Self::TooManyAttempts(wait) = self {
            let seconds = (wait + 999) / 1000;
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}
