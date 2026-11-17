use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::{AppError, AppState, Auth};
use crate::app::auth::throttle::{Attempt, Client};
use crate::app::auth::{lockout, password};
use crate::app::store::{Activity, MAX_EMAIL_BYTES, is_valid_email, normalize_email};
use crate::time::now_millis;

#[derive(Deserialize)]
pub struct SignIn {
    email: String,
    password: String,
}

/// The signed-in user, the CSRF token, the idle time, and the time left until the session goes
/// idle: input in any tab of the browser keeps the one session alive.
pub async fn current(auth: Auth, State(state): State<AppState>) -> Result<Response, AppError> {
    let session = state.db.session(&auth.token)?.ok_or(AppError::SignedOut)?;
    let idle_ms = state.serve.sessions.idle_ms;
    let idle_left_ms = (idle_ms - (now_millis() - session.last_seen_at)).max(0);
    Ok(Json(json!({
        "email": session.email,
        "csrf": session.csrf,
        "idle_ms": idle_ms,
        "idle_left_ms": idle_left_ms,
    }))
    .into_response())
}

/// Checks the password and starts a session. Unknown emails, disabled users and wrong
/// passwords get the same answer after the same work. Each attempt counts against the client and
/// the email before the password check, so parallel attempts cannot pass the limits.
pub async fn sign_in(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SignIn>,
) -> Result<Response, AppError> {
    let email = normalize_email(&body.email);
    let ip = state.serve.client_ip(peer.ip(), &headers);
    let client = Client::of(ip);
    let now = now_millis();
    let throttle = state.throttle.as_deref();
    let attempt = match throttle
        .map(|throttle| throttle.begin(client, now))
        .transpose()
    {
        Ok(attempt) => attempt,
        Err(wait) => {
            log_failure(client, shown(&email), "the address must wait");
            return Err(AppError::TooManyAttempts(wait));
        }
    };
    if !is_valid_email(&email) {
        failed(&state, attempt, client, shown(&email), "not an email");
        return Err(AppError::BadSignIn);
    }
    let failures = match lockout::begin_attempt(&state.db, &email, now)? {
        Ok(failures) => failures,
        Err(wait) => {
            failed(
                &state,
                attempt,
                client,
                &email,
                "refused: the account must wait",
            );
            return Err(AppError::TooManyAttempts(wait));
        }
    };

    if !is_password_right(&state, &email, body.password).await? {
        failed(&state, attempt, client, &email, &failure_detail(failures));
        return Err(AppError::BadSignIn);
    }
    state.db.clear_login_failures(&email)?;
    let now = now_millis();
    let limits = state.serve.sessions;
    state
        .db
        .delete_old_sessions(now - limits.absolute_ms, now - limits.idle_ms)?;
    let (token, csrf) = state.db.create_session(&email, now)?;
    let place = if state.serve.is_website() {
        format!("from {ip}")
    } else {
        "this computer".to_owned()
    };
    state.record(&email, Activity::SignIn, None, &place);
    let cookies = state.cookies();
    let body = Json(json!({"email": email, "csrf": csrf, "idle_ms": limits.idle_ms}));
    let set_cookies = AppendHeaders([
        (header::SET_COOKIE, cookies.session(&token)),
        (header::SET_COOKIE, cookies.email(&email)),
    ]);
    Ok((set_cookies, body).into_response())
}

/// Whether the user exists, is enabled and has this password. Checks a dummy hash otherwise, so
/// every answer takes the same time.
async fn is_password_right(
    state: &AppState,
    email: &str,
    candidate: String,
) -> Result<bool, AppError> {
    let user = state.db.password_hash(email)?;
    let enabled = matches!(user, Some((_, false)));
    let stored = match user {
        Some((hash, false)) => hash,
        _ => password::dummy_hash().to_owned(),
    };
    let permit = state
        .hashing
        .clone()
        .acquire_owned()
        .await
        .expect("the semaphore stays open");
    let matches = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        candidate.chars().count() <= password::MAX_LENGTH && password::verify(&candidate, &stored)
    })
    .await
    .unwrap_or(false);
    Ok(enabled && matches)
}

/// Keeps a failed sign-in in the client's count, logs it and writes it to the activity log.
fn failed(state: &AppState, attempt: Option<Attempt>, client: Client, email: &str, detail: &str) {
    if let Some(attempt) = attempt {
        attempt.fail(now_millis());
    }
    log_failure(client, email, detail);
    state.record(email, Activity::FailedSignIn, None, detail);
}

/// One line per failed sign-in, for fail2ban. The email is quoted so it cannot break the line.
fn log_failure(client: Client, email: &str, detail: &str) {
    tracing::warn!("upkeep: failed sign-in from {client} for {email:?}: {detail}");
}

/// The email cut to [`MAX_EMAIL_BYTES`], for the log and the activity log.
fn shown(email: &str) -> &str {
    &email[..email.floor_char_boundary(MAX_EMAIL_BYTES)]
}

fn failure_detail(failures: u32) -> String {
    match lockout::wait_after(failures) {
        None => format!("attempt {failures} of {}", lockout::FREE_ATTEMPTS),
        Some(wait) => format!("attempt {failures}, must wait {} s", wait / 1000),
    }
}

/// Marks the session as used. The page calls it after user input and while the user watches a
/// running run.
pub async fn ping(_auth: Auth) -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Ends the session. The email cookie stays, so the screen asks only for the password.
pub async fn lock(auth: Auth, State(state): State<AppState>) -> Result<Response, AppError> {
    state.db.delete_session(&auth.token)?;
    let cookies = [(header::SET_COOKIE, state.cookies().clear_session())];
    Ok((StatusCode::NO_CONTENT, cookies).into_response())
}

/// Ends the session and forgets the email.
pub async fn sign_out(auth: Auth, State(state): State<AppState>) -> Result<Response, AppError> {
    state.db.delete_session(&auth.token)?;
    let cookies = state.cookies();
    let cookies = AppendHeaders([
        (header::SET_COOKIE, cookies.clear_session()),
        (header::SET_COOKIE, cookies.clear_email()),
    ]);
    Ok((StatusCode::NO_CONTENT, cookies).into_response())
}
