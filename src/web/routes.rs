use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{AppendHeaders, Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;

use super::oauth::LOGIN_TTL_SECS;
use super::session::{AccessError, SESSION_TTL_SECS, Session};
use super::{WebError, WebState, api};
use crate::time::now_unix;

const INDEX: &str = include_str!("assets/index.html");
const SCRIPT: &str = include_str!("assets/app.js");
const STYLE: &str = include_str!("assets/app.css");

/// The page, its sign-in and its `/api`. Every response carries the security headers.
pub fn router(state: WebState) -> Router {
    let api = api::router().layer(middleware::from_fn_with_state(
        state.clone(),
        require_session,
    ));
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/app.js", get(|| asset("text/javascript", SCRIPT)))
        .route("/app.css", get(|| asset("text/css", STYLE)))
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .nest("/api", api)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let fixed = [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; frame-ancestors 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::CACHE_CONTROL, "no-store"),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Lets `/api` requests through only with a live session, and changes only with its CSRF token.
async fn require_session(
    State(state): State<WebState>,
    mut request: Request,
    next: Next,
) -> Response {
    match authorize(&state, request.method(), request.headers()) {
        Ok(session) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

fn authorize(state: &WebState, method: &Method, headers: &HeaderMap) -> Result<Session, WebError> {
    let id = state
        .session_cookie
        .read(headers)
        .ok_or(AccessError::SignedOut)?;
    let session = state
        .db
        .session(id, now_unix())?
        .ok_or(AccessError::SignedOut)?;
    if !matches!(*method, Method::GET | Method::HEAD) {
        let presented = headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok());
        session.check_csrf(presented)?;
    }
    Ok(session)
}

/// Sends the browser to Google, with a cookie that ties the answer to this browser.
async fn login(State(state): State<WebState>) -> Response {
    match state.logins.start(now_unix()) {
        Ok(login) => {
            let cookie = state.login_cookie.set(&login.state, LOGIN_TTL_SECS);
            let target = state.google.authorize_url(&login);
            ([(header::SET_COOKIE, cookie)], Redirect::to(&target)).into_response()
        }
        Err(error) => notice(
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again later",
            &error.to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// Google's answer: checks the sign-in, the allow list, then starts a session.
async fn callback(
    State(state): State<WebState>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    let (Some(code), Some(login_state)) = (query.code, query.state) else {
        return notice(
            StatusCode::BAD_REQUEST,
            "Sign-in cancelled",
            "Google sent no sign-in.",
        );
    };
    let same_browser = state.login_cookie.read(&headers) == Some(login_state.as_str());
    let pending = state.logins.finish(&login_state, now_unix());
    let Some(pending) = pending.filter(|_| same_browser) else {
        return notice(
            StatusCode::BAD_REQUEST,
            "Sign-in expired",
            "This sign-in is old or was started in another browser. Please sign in again.",
        );
    };
    let email = match state.google.email(&code, &pending).await {
        Ok(email) => email,
        Err(error) => {
            tracing::warn!(%error, "sign-in failed");
            let detail = error.to_string();
            return match state
                .db
                .record("(unknown)", "sign-in refused", None, &detail)
            {
                Ok(()) => notice(StatusCode::BAD_GATEWAY, "Sign-in failed", &detail),
                Err(error) => WebError::from(error).into_response(),
            };
        }
    };
    match start_session(&state, &email) {
        Ok(Some(cookie)) => {
            let cookies = AppendHeaders([
                (header::SET_COOKIE, cookie),
                (header::SET_COOKIE, state.login_cookie.clear()),
            ]);
            (cookies, Redirect::to("/")).into_response()
        }
        Ok(None) => notice(
            StatusCode::FORBIDDEN,
            "Not allowed",
            "This Google account is not on the list of allowed users. Ask an admin to add it.",
        ),
        Err(error) => error.into_response(),
    }
}

/// The session cookie for an allowed email; `None` when the email is not on the list.
fn start_session(state: &WebState, email: &str) -> Result<Option<String>, WebError> {
    if state.db.role(email)?.is_none() {
        tracing::warn!(email, "sign-in refused");
        state
            .db
            .record(email, "sign-in refused", None, "not on the list")?;
        return Ok(None);
    }
    let id = state.db.create_session(email, now_unix())?;
    state.db.record(email, "sign-in", None, "")?;
    tracing::info!(email, "signed in");
    Ok(Some(state.session_cookie.set(&id, SESSION_TTL_SECS)))
}

/// A plain page with a title, one sentence and a way back.
fn notice(status: StatusCode, title: &str, text: &str) -> Response {
    let escape = crate::monitor::message::escape;
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><link rel=\"stylesheet\" href=\"/app.css\"></head>\
         <body><main class=\"login\"><b>{title}</b><p class=\"muted\">{text}</p>\
         <a class=\"gbtn\" href=\"/\">Back to Upkeep</a></main></body></html>",
        title = escape(title),
        text = escape(text),
    );
    (status, Html(page)).into_response()
}
