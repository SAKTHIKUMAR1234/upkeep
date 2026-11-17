use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use super::runs::{cursor, given};
use super::{AppError, AppState, Auth};
use crate::app::store::{Activity, ActivityFilter};

#[derive(Deserialize)]
pub struct ActivityQuery {
    user: Option<String>,
    server: Option<String>,
    action: Option<String>,
    before: Option<String>,
}

/// One page of the activity log: `{"items": [...], "next": "<cursor>" | null}`.
pub async fn list(
    _auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<Value>, AppError> {
    let action = given(&query.action)
        .map(|text| {
            Activity::parse(text)
                .ok_or_else(|| AppError::Invalid(format!("unknown action {text:?}")))
        })
        .transpose()?;
    let filter = ActivityFilter {
        user: given(&query.user).map(str::to_owned),
        server: given(&query.server).map(str::to_owned),
        action,
    };
    let page = state.db.activity(&filter, cursor(&query.before)?)?;
    Ok(Json(json!(page)))
}

/// The choices of the filter menus: servers, operation folders and users.
pub async fn filters(_auth: Auth, State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let servers: Vec<Value> = state
        .db
        .servers()?
        .into_iter()
        .map(|server| json!({"id": server.id, "name": server.name}))
        .collect();
    let users: Vec<String> = state
        .db
        .users()?
        .into_iter()
        .map(|user| user.email)
        .collect();
    Ok(Json(json!({
        "servers": servers,
        "operations": state.db.operation_folders()?,
        "users": users,
    })))
}
