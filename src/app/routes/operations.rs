use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AppError, AppState, Auth};
use crate::app::operations::{self, OperationError, ROOT, Scripts, is_valid_folder, title_of};
use crate::app::runs::ssh::Ssh;
use crate::app::store::{Activity, RunRecord, Server};

const LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize)]
struct OperationView {
    folder: String,
    title: String,
    has_check: bool,
    last_run: Option<RunRecord>,
}

fn ssh(state: &AppState, server: &Server) -> Ssh {
    Ssh::new(state.executor.config(), &server.fields())
}

/// Stores whether SSH worked, from the result of a remote call.
fn note_reachable<T>(state: &AppState, server: &Server, result: &Result<T, OperationError>) {
    let reachable = operations::reachability(result);
    if reachable.is_some() && reachable != server.reachable {
        let stored = state.db.set_reachable(server.id, reachable == Some(true));
        if let Err(error) = stored {
            tracing::warn!(%error, "cannot store the server state");
        }
    }
}

/// The operations on the server. When the server cannot be reached, the last known list comes
/// back with `connected: false` and the reason.
pub async fn list(
    _auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let server = state.server(id)?;
    let listed = operations::list(&ssh(&state, &server), LIST_TIMEOUT).await;
    note_reachable(&state, &server, &listed);
    let error = match listed {
        Ok(found) => {
            state.db.replace_operations(id, &found)?;
            None
        }
        Err(OperationError::NoFolder) => {
            state.db.replace_operations(id, &[])?;
            None
        }
        Err(error) => Some(error.to_string()),
    };
    let views = state
        .db
        .cached_operations(id)?
        .into_iter()
        .map(|operation| {
            Ok(OperationView {
                last_run: state.db.last_run(id, &operation.folder)?,
                title: operation
                    .title
                    .unwrap_or_else(|| title_of(&operation.folder)),
                folder: operation.folder,
                has_check: operation.has_check,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(Json(
        json!({"connected": error.is_none(), "error": error, "operations": views}),
    ))
}

#[derive(Deserialize)]
pub struct NewOperation {
    title: String,
    folder: String,
    #[serde(flatten)]
    scripts: Scripts,
}

fn checked_title(title: &str) -> Result<&str, AppError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
        return Err(AppError::Invalid(
            "Enter a name of 1 to 80 characters.".into(),
        ));
    }
    Ok(title)
}

fn checked_folder(folder: &str) -> Result<&str, AppError> {
    is_valid_folder(folder)
        .then_some(folder)
        .ok_or(AppError::Operation(OperationError::InvalidFolder))
}

fn files_text(scripts: &Scripts) -> &'static str {
    if scripts.has_check() {
        "run.sh + check.sh"
    } else {
        "run.sh"
    }
}

pub async fn create(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<NewOperation>,
) -> Result<Response, AppError> {
    let server = state.server(id)?;
    let title = checked_title(&body.title)?;
    let folder = checked_folder(&body.folder)?;
    let created = operations::create(&ssh(&state, &server), folder, &body.scripts).await;
    note_reachable(&state, &server, &created);
    created?;
    state
        .db
        .save_operation(id, folder, Some(title), body.scripts.has_check())?;
    let detail = format!("{ROOT}/{folder} · {}", files_text(&body.scripts));
    state.record(
        &auth.email,
        Activity::OperationCreate,
        Some(&server.name),
        &detail,
    );
    let view = json!({"folder": folder, "title": title, "has_check": body.scripts.has_check()});
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

/// The current scripts, read from the server.
pub async fn show(
    _auth: Auth,
    State(state): State<AppState>,
    Path((id, folder)): Path<(i64, String)>,
) -> Result<Json<Value>, AppError> {
    let server = state.server(id)?;
    let folder = checked_folder(&folder)?;
    let scripts = operations::read(&ssh(&state, &server), folder).await;
    note_reachable(&state, &server, &scripts);
    let scripts = scripts?;
    let title = stored_title(&state, id, folder)?;
    Ok(Json(json!({
        "folder": folder,
        "title": title,
        "run_sh": scripts.run_sh,
        "check_sh": scripts.check_sh,
    })))
}

/// The name the app stored for a folder, or one made from the folder name.
fn stored_title(state: &AppState, server_id: i64, folder: &str) -> Result<String, AppError> {
    Ok(state
        .db
        .cached_operations(server_id)?
        .into_iter()
        .find(|operation| operation.folder == folder)
        .and_then(|operation| operation.title)
        .unwrap_or_else(|| title_of(folder)))
}

#[derive(Deserialize)]
pub struct EditOperation {
    title: Option<String>,
    #[serde(flatten)]
    scripts: Scripts,
}

/// Saves changed scripts and records each one in the activity log with a diff summary.
pub async fn edit(
    auth: Auth,
    State(state): State<AppState>,
    Path((id, folder)): Path<(i64, String)>,
    Json(body): Json<EditOperation>,
) -> Result<Json<Value>, AppError> {
    let server = state.server(id)?;
    let folder = checked_folder(&folder)?;
    let title = body.title.as_deref().map(checked_title).transpose()?;
    let ssh = ssh(&state, &server);
    let old = operations::read(&ssh, folder).await;
    note_reachable(&state, &server, &old);
    let changes = operations::update(&ssh, folder, &old?, &body.scripts).await?;
    let old_title = stored_title(&state, id, folder)?;
    state
        .db
        .save_operation(id, folder, title, body.scripts.has_check())?;
    if let Some(title) = title.filter(|title| *title != old_title) {
        let detail = format!("{folder}: name {old_title} → {title}");
        state.record(
            &auth.email,
            Activity::OperationEdit,
            Some(&server.name),
            &detail,
        );
    }
    for (file, summary) in &changes {
        let detail = format!("{folder}/{file} · {summary}");
        state.record(
            &auth.email,
            Activity::ScriptEdit,
            Some(&server.name),
            &detail,
        );
    }
    let changed: Vec<Value> = changes
        .iter()
        .map(|(file, summary)| json!({"file": file, "summary": summary}))
        .collect();
    Ok(Json(json!({"changes": changed})))
}

/// Removes the folder from the server. Refused while the operation runs.
pub async fn delete(
    auth: Auth,
    State(state): State<AppState>,
    Path((id, folder)): Path<(i64, String)>,
) -> Result<StatusCode, AppError> {
    let server = state.server(id)?;
    let folder = checked_folder(&folder)?;
    if let Some(run) = state
        .db
        .active_run(id)?
        .filter(|run| run.operation == folder)
    {
        return Err(AppError::Conflict(format!(
            "Run #{} of this operation is active. Stop it before you delete the operation.",
            run.id
        )));
    }
    let deleted = operations::delete(&ssh(&state, &server), folder).await;
    note_reachable(&state, &server, &deleted);
    deleted?;
    state.db.forget_operation(id, folder)?;
    state.record(
        &auth.email,
        Activity::OperationDelete,
        Some(&server.name),
        &format!("{ROOT}/{folder}"),
    );
    Ok(StatusCode::NO_CONTENT)
}
