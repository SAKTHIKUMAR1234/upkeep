use std::path::Path;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use super::{AppError, AppState, Auth};
use crate::app::runs::ssh::check_key;
use crate::app::servers::{ServerInput, test_connection};
use crate::app::store::{Activity, RunRecord, Server, ServerFields, is_unique_violation};
use crate::time::now_millis;

const THIRTY_DAYS_MS: i64 = 30 * 24 * 3_600_000;

/// A server with the id of its active run.
#[derive(Serialize)]
struct ServerView {
    #[serde(flatten)]
    server: Server,
    running_run: Option<i64>,
}

fn views(servers: Vec<Server>, active: &[RunRecord]) -> Vec<ServerView> {
    servers
        .into_iter()
        .map(|server| ServerView {
            running_run: active
                .iter()
                .find(|run| run.server_id == server.id)
                .map(|run| run.id),
            server,
        })
        .collect()
}

pub async fn list(_auth: Auth, State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let active = state.db.active_runs()?;
    Ok(Json(
        json!({"servers": views(state.db.servers()?, &active)}),
    ))
}

/// The counts on the overview: servers, active runs, and runs of the last 30 days by result.
pub async fn overview(_auth: Auth, State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let counts = state.db.status_counts(now_millis() - THIRTY_DAYS_MS)?;
    let total: i64 = counts.iter().map(|(_, count)| count).sum();
    let count_of = |status: &str| {
        counts
            .iter()
            .find(|(name, _)| name == status)
            .map_or(0, |(_, count)| *count)
    };
    Ok(Json(json!({
        "servers": state.db.servers()?.len(),
        "running": state.db.active_runs()?.len(),
        "runs_30d": total,
        "succeeded_30d": count_of("succeeded"),
        "failed_30d": count_of("failed") + count_of("check_failed"),
    })))
}

/// Validates the dialog fields and the key file.
fn checked_fields(input: &ServerInput) -> Result<ServerFields, AppError> {
    let fields = input
        .validate()
        .map_err(|error| AppError::Invalid(error.to_string()))?;
    check_key(Path::new(&fields.key_path)).map_err(|error| AppError::Invalid(error.to_string()))?;
    Ok(fields)
}

fn name_taken(error: rusqlite::Error, name: &str) -> AppError {
    if is_unique_violation(&error) {
        AppError::Conflict(format!("A server named {name:?} exists already."))
    } else {
        error.into()
    }
}

pub async fn add(
    auth: Auth,
    State(state): State<AppState>,
    Json(input): Json<ServerInput>,
) -> Result<Response, AppError> {
    let fields = checked_fields(&input)?;
    let id = state
        .db
        .add_server(&fields, now_millis())
        .map_err(|error| name_taken(error, &fields.name))?;
    let detail = format!("{}@{}", fields.ssh_user, fields.address);
    state.record(
        &auth.email,
        Activity::ServerAdd,
        Some(&fields.name),
        &detail,
    );
    Ok((StatusCode::CREATED, Json(state.server(id)?)).into_response())
}

/// "Test connection" in the add and edit dialogs, before the server is saved.
pub async fn test(
    _auth: Auth,
    State(state): State<AppState>,
    Json(mut input): Json<ServerInput>,
) -> Result<Json<Value>, AppError> {
    if input.name.trim().is_empty() {
        input.name = "new server".into();
    }
    let fields = input
        .validate()
        .map_err(|error| AppError::Invalid(error.to_string()))?;
    let result = test_connection(state.executor.config(), &fields).await;
    Ok(Json(json!(result)))
}

/// A server with the figures of its page.
pub async fn show(
    _auth: Auth,
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<Value>, AppError> {
    let server = state.server(id)?;
    let stats = state.db.server_stats(id, now_millis() - THIRTY_DAYS_MS)?;
    let operations = state.db.cached_operations(id)?.len();
    let running_run = state.db.active_run(id)?.map(|run| run.id);
    Ok(Json(json!({
        "server": server,
        "operations": operations,
        "runs_30d": stats.runs_since,
        "last_run": stats.last_run,
        "running_run": running_run,
    })))
}

pub async fn edit(
    auth: Auth,
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
    Json(input): Json<ServerInput>,
) -> Result<Json<Server>, AppError> {
    let old = state.server(id)?;
    let fields = checked_fields(&input)?;
    state
        .db
        .update_server(id, &fields)
        .map_err(|error| name_taken(error, &fields.name))?;
    let detail = changed_fields(&old.fields(), &fields);
    state.record(
        &auth.email,
        Activity::ServerEdit,
        Some(&fields.name),
        &detail,
    );
    Ok(Json(state.server(id)?))
}

fn changed_fields(old: &ServerFields, new: &ServerFields) -> String {
    let changed: Vec<&str> = [
        ("name", old.name != new.name),
        ("address", old.address != new.address),
        ("port", old.port != new.port),
        ("SSH user", old.ssh_user != new.ssh_user),
        ("key", old.key_path != new.key_path),
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect();
    if changed.is_empty() {
        "no changes".into()
    } else {
        format!("changed {}", changed.join(", "))
    }
}

pub async fn delete(
    auth: Auth,
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let server = state.server(id)?;
    if let Some(run) = state.db.active_run(id)? {
        return Err(AppError::Conflict(format!(
            "Run #{} is active on this server. Stop it before you delete the server.",
            run.id
        )));
    }
    state.db.delete_server(id)?;
    let detail = format!("{}@{}", server.ssh_user, server.address);
    state.record(
        &auth.email,
        Activity::ServerDelete,
        Some(&server.name),
        &detail,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Private key files in `~/.ssh`, offered in the key field.
pub async fn ssh_keys(_auth: Auth) -> Json<Value> {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let folder = Path::new(&home).join(".ssh");
    let mut keys: Vec<String> = std::fs::read_dir(&folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.with_extension("pub").exists()
                && path.extension().is_none_or(|extension| extension != "pub")
        })
        .map(|path| path.display().to_string())
        .collect();
    keys.sort();
    Json(json!({"keys": keys}))
}
