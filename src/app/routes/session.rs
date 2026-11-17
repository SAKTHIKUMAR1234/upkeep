use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::{AppError, AppState, Auth};
use crate::app::auth::session::{ABSOLUTE_MS, IDLE_MS, clear_cookie, set_cookie};
use crate::app::auth::{lockout, password};
use crate::app::store::{Activity, normalize_email};
use crate::time::now_millis;

#[derive(Deserialize)]
pub struct SignIn {
    email: String,
    password: String,
}

/// The signed-in user and the CSRF token for the page.
pub async fn current(auth: Auth, State(state): State<AppState>) -> Result<Response, AppError> {
    let session = state.db.session(&auth.token)?.ok_or(AppError::SignedOut)?;
    Ok(Json(json!({"email": session.email, "csrf": session.csrf})).into_response())
}

/// Checks the password and starts a session. Unknown emails, disabled users and wrong
/// passwords get the same answer after the same work.
pub async fn sign_in(
    State(state): State<AppState>,
    Json(body): Json<SignIn>,
) -> Result<Response, AppError> {
    let email = normalize_email(&body.email);
    let now = now_millis();
    if let Some(wait) = lockout::remaining_wait(&state.db, &email, now)? {
        state.record(
            &email,
            Activity::FailedSignIn,
            None,
            "refused: the account must wait",
        );
        return Err(AppError::TooManyAttempts(wait));
    }

    let user = state.db.password_hash(&email)?;
    let enabled = matches!(user, Some((_, false)));
    let stored = match user {
        Some((hash, false)) => hash,
        _ => password::dummy_hash().to_owned(),
    };
    let candidate = body.password;
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

    if !(enabled && matches) {
        let failures = lockout::record_failure(&state.db, &email, now_millis())?;
        state.record(
            &email,
            Activity::FailedSignIn,
            None,
            &failure_detail(failures),
        );
        return Err(AppError::BadSignIn);
    }
    state.db.clear_login_failures(&email)?;
    let now = now_millis();
    state
        .db
        .delete_old_sessions(now - ABSOLUTE_MS, now - IDLE_MS)?;
    let (token, csrf) = state.db.create_session(&email, now)?;
    state.record(&email, Activity::SignIn, None, "this computer");
    let body = Json(json!({"email": email, "csrf": csrf}));
    Ok(([(header::SET_COOKIE, set_cookie(&token))], body).into_response())
}

fn failure_detail(failures: u32) -> String {
    match lockout::wait_after(failures) {
        None => format!("attempt {failures} of {}", lockout::FREE_ATTEMPTS),
        Some(wait) => format!("attempt {failures}, must wait {} s", wait / 1000),
    }
}

/// Ends the session, for both "Lock" and "Sign out".
pub async fn sign_out(auth: Auth, State(state): State<AppState>) -> Result<Response, AppError> {
    state.db.delete_session(&auth.token)?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, clear_cookie())],
    )
        .into_response())
}
