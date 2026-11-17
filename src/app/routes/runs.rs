use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AppError, AppState, Auth};
use crate::app::runs::output::{MAX_CHUNK, read_chunk};
use crate::app::store::{Activity, CachedOperation, Cursor, RunFilter, RunRecord, RunStatus};

pub async fn start(
    auth: Auth,
    State(state): State<AppState>,
    Path((id, folder)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    let server = state.server(id)?;
    let operation = state
        .db
        .cached_operations(id)?
        .into_iter()
        .find(|operation| operation.folder == folder)
        .unwrap_or(CachedOperation {
            folder,
            title: None,
            has_check: false,
        });
    let run = state.executor.start(&server, &operation, &auth.email)?;
    let detail = format!("{} · run #{}", run.title, run.id);
    state.record(&auth.email, Activity::Run, Some(&server.name), &detail);
    Ok((StatusCode::CREATED, Json(run)).into_response())
}

/// History filters as the page sends them. Empty values mean "all".
#[derive(Deserialize, Default)]
pub struct RunQuery {
    server: Option<String>,
    operation: Option<String>,
    user: Option<String>,
    result: Option<String>,
    since: Option<String>,
    q: Option<String>,
    before: Option<String>,
}

pub fn given(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn number(value: &Option<String>, name: &str) -> Result<Option<i64>, AppError> {
    given(value)
        .map(|text| {
            text.parse()
                .map_err(|_| AppError::Invalid(format!("{name} must be a number")))
        })
        .transpose()
}

impl RunQuery {
    fn filter(&self) -> Result<RunFilter, AppError> {
        let status = given(&self.result)
            .map(|text| {
                RunStatus::parse(text)
                    .ok_or_else(|| AppError::Invalid(format!("unknown result {text:?}")))
            })
            .transpose()?;
        Ok(RunFilter {
            server_id: number(&self.server, "server")?,
            operation: given(&self.operation).map(str::to_owned),
            user: given(&self.user).map(str::to_owned),
            status,
            since: number(&self.since, "since")?,
            search: given(&self.q).map(str::to_owned),
        })
    }
}

/// Parses `before`, the cursor of the next page.
pub fn cursor(before: &Option<String>) -> Result<Cursor, AppError> {
    match given(before) {
        None => Ok(Cursor::start()),
        Some(text) => Cursor::parse(text)
            .ok_or_else(|| AppError::Invalid("before is not a page cursor".into())),
    }
}

/// One page of history: `{"items": [...], "next": "<cursor>" | null}`.
pub async fn list(
    _auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<RunQuery>,
) -> Result<Json<Value>, AppError> {
    let page = state.db.runs(&query.filter()?, cursor(&query.before)?)?;
    Ok(Json(json!(page)))
}

/// The filtered history as CSV, read page by page so the size does not matter.
pub async fn export(
    _auth: Auth,
    State(state): State<AppState>,
    Query(query): Query<RunQuery>,
) -> Result<Response, AppError> {
    let filter = query.filter()?;
    let start = Some(Cursor::start());
    let pages = futures_util::stream::unfold(start, move |cursor| {
        let (db, filter) = (state.db.clone(), filter.clone());
        async move {
            let page = db.runs(&filter, cursor?);
            let (text, next) = match page {
                Ok(page) => (
                    page.items.iter().map(csv_line).collect::<String>(),
                    page.next.as_deref().and_then(Cursor::parse),
                ),
                Err(error) => {
                    tracing::error!(%error, "history export stopped");
                    return None;
                }
            };
            Some((Ok::<_, std::io::Error>(Bytes::from(text)), next))
        }
    });
    let header_line = futures_util::stream::once(async {
        Ok(Bytes::from_static(
            b"run,server,operation,result,user,started_at,finished_at,exit_code\n",
        ))
    });
    let body = Body::from_stream(futures_util::StreamExt::chain(header_line, pages));
    let headers = [
        (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"upkeep-history.csv\"",
        ),
    ];
    Ok((headers, body).into_response())
}

fn csv_line(run: &RunRecord) -> String {
    let field = |text: &str| {
        // A leading = + - @ would run as a formula in a spreadsheet.
        let text = if text.starts_with(['=', '+', '-', '@']) {
            format!("'{text}")
        } else {
            text.to_owned()
        };
        format!("\"{}\"", text.replace('"', "\"\""))
    };
    let time = |at: Option<i64>| {
        at.map(|at| crate::time::format_unix(at / 1000))
            .unwrap_or_default()
    };
    format!(
        "{},{},{},{},{},{},{},{}\n",
        run.id,
        field(&run.server_name),
        field(&run.operation),
        run.status.as_str(),
        field(&run.user),
        time(Some(run.started_at)),
        time(run.finished_at),
        run.exit_code
            .map(|code| code.to_string())
            .unwrap_or_default(),
    )
}

pub async fn show(
    _auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<RunRecord>, AppError> {
    Ok(Json(state.db.run(id)?.ok_or(AppError::NotFound("run"))?))
}

#[derive(Deserialize)]
pub struct OutputQuery {
    #[serde(default)]
    offset: u64,
    limit: Option<u64>,
    tail: Option<u64>,
}

/// A byte range of the run's output: `offset` and `limit`, or the last `tail` bytes.
pub async fn output(
    _auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<OutputQuery>,
) -> Result<Response, AppError> {
    state.db.run(id)?.ok_or(AppError::NotFound("run"))?;
    let path = state.executor.config().run_log(id);
    let limit = query.limit.unwrap_or(MAX_CHUNK);
    let chunk =
        tokio::task::spawn_blocking(move || read_chunk(&path, query.offset, limit, query.tail))
            .await
            .map_err(|error| AppError::Invalid(error.to_string()))?
            .map_err(|error| AppError::Run(error.into()))?;
    Ok(Json(chunk).into_response())
}

pub async fn stop(
    auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let run = state.db.run(id)?.ok_or(AppError::NotFound("run"))?;
    state.executor.stop(id, &auth.email)?;
    let detail = format!("{} · run #{id}", run.title);
    state.record(&auth.email, Activity::Stop, Some(&run.server_name), &detail);
    Ok(StatusCode::ACCEPTED)
}
