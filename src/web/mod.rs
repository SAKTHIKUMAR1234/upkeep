pub mod api;
pub mod oauth;
pub mod routes;
pub mod runners;
pub mod session;
pub mod settings;
pub mod users;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::net::TcpListener;

use crate::api::auth::Token;
use oauth::{Google, Logins};
use runners::{RunnerError, Runners};
use session::{AccessError, Cookie};
use settings::WebSettings;
use users::WebDb;

/// Everything the web handlers share.
#[derive(Clone)]
pub struct WebState {
    pub db: Arc<WebDb>,
    pub logins: Arc<Logins>,
    pub google: Arc<Google>,
    pub runners: Arc<Runners>,
    /// The session cookie.
    pub session_cookie: Cookie,
    /// Ties a sign-in to the browser that started it.
    pub login_cookie: Cookie,
}

impl WebState {
    /// Reads the Google secret and the runner tokens, and opens the database.
    pub fn new(settings: &WebSettings, state_dir: &Path) -> anyhow::Result<Self> {
        let client_secret = Token::load(&settings.google_client_secret_file)?;
        let database = settings.database_path(state_dir);
        if let Some(parent) = database.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let db = WebDb::open(&database)
            .with_context(|| format!("cannot open {}", database.display()))?;
        Ok(Self {
            db: Arc::new(db),
            logins: Arc::default(),
            google: Arc::new(Google::new(settings, client_secret)),
            runners: Arc::new(Runners::new(settings)?),
            session_cookie: Cookie::new("upkeep", settings.cookie_secure),
            login_cookie: Cookie::new("upkeep-login", settings.cookie_secure),
        })
    }
}

/// An `/api` error as `{"error": "..."}` with its status.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error("no server named {0:?}")]
    UnknownHost(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("An update is already running on this server")]
    Busy,
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Access(AccessError::SignedOut) => StatusCode::UNAUTHORIZED,
            Self::Access(_) => StatusCode::FORBIDDEN,
            Self::UnknownHost(_) | Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Busy => StatusCode::CONFLICT,
            // The runner refused the web page's own token: not the browser's sign-in.
            Self::Runner(RunnerError::Status {
                status: 401 | 403, ..
            }) => StatusCode::BAD_GATEWAY,
            Self::Runner(RunnerError::Status { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::Runner(RunnerError::Unreachable(_)) => StatusCode::BAD_GATEWAY,
            Self::Database(error) => {
                tracing::error!(%error, "web database");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let message = match self {
            Self::Database(_) => "database error".to_owned(),
            other => other.to_string(),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

/// `upkeep web serve`: serves the web page until SIGTERM or Ctrl-C.
pub async fn serve(settings_file: &Path, state_dir: &Path) -> anyhow::Result<()> {
    let settings = WebSettings::load(settings_file)?;
    let state = WebState::new(&settings, state_dir)?;
    let listener = TcpListener::bind(settings.listen)
        .await
        .with_context(|| format!("cannot listen on {}", settings.listen))?;
    tracing::info!(address = %settings.listen, public_url = %settings.public_url, "web page listening");
    axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(crate::api::shutdown_signal())
        .await?;
    Ok(())
}
