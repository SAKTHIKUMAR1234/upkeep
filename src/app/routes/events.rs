use std::convert::Infallible;
use std::path::Path as FilePath;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::{Receiver, error::RecvError};

use super::{AppError, AppState, Auth};
use crate::app::runs::RunEvent;
use crate::app::runs::output::{MAX_CHUNK, read_chunk};
use crate::app::store::{RunRecord, RunStatus};

#[derive(Deserialize)]
pub struct EventsQuery {
    /// The output byte the page already has.
    #[serde(default)]
    from: u64,
}

fn run_event(run: &RunRecord) -> Event {
    Event::default()
        .event("run")
        .json_data(run)
        .expect("a run serializes")
}

fn output_event(start: u64, end: u64, text: &str) -> Event {
    let data = serde_json::json!({"start": start, "end": end, "text": text});
    Event::default()
        .event("output")
        .id(end.to_string())
        .json_data(data)
        .expect("output serializes")
}

/// The output written so far, from `from`, as events.
fn catch_up(path: &FilePath, from: u64) -> (Vec<Event>, u64) {
    let mut events = Vec::new();
    let mut offset = from;
    while let Ok(chunk) = read_chunk(path, offset, MAX_CHUNK, None) {
        if chunk.end <= offset {
            return (events, chunk.size.max(offset));
        }
        events.push(output_event(chunk.start, chunk.end, &chunk.text));
        offset = chunk.end;
    }
    (events, offset)
}

/// A run's state, then its output from `from` (or `Last-Event-ID`), then live changes until the
/// run ends.
pub async fn run_events(
    _auth: Auth,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let receiver = state.executor.subscribe(id);
    let run = state.db.run(id)?.ok_or(AppError::NotFound("run"))?;
    let resumed = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok()?.parse().ok())
        .unwrap_or(0);
    let path = state.executor.config().run_log(id);
    let from = query.from.max(resumed);
    let (mut events, written) = tokio::task::spawn_blocking(move || catch_up(&path, from))
        .await
        .unwrap_or_default();
    events.insert(0, run_event(&run));

    let initial = stream::iter(events.into_iter().map(Ok));
    let live = match receiver.filter(|_| run.status == RunStatus::Running) {
        Some(receiver) => live_events(receiver, written).boxed(),
        None => stream::empty().boxed(),
    };
    Ok(Sse::new(initial.chain(live).boxed()).keep_alive(KeepAlive::default()))
}

/// Events of an active run after byte `written`, which the catch-up already sent.
fn live_events(
    receiver: Receiver<RunEvent>,
    written: u64,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    stream::unfold(receiver, move |mut receiver| async move {
        loop {
            let event = match receiver.recv().await {
                Ok(RunEvent::Run(run)) => run_event(&run),
                Ok(RunEvent::Output(output)) if output.start >= written => {
                    output_event(output.start, output.end, &output.text)
                }
                Ok(RunEvent::Output(_)) => continue,
                // A slow page reconnects and resumes from its last event.
                Err(RecvError::Lagged(_) | RecvError::Closed) => return None,
            };
            return Some((Ok(event), receiver));
        }
    })
}

/// Runs that start or end on any server, for the sidebar and the lists.
pub async fn changes(_auth: Auth, State(state): State<AppState>) -> impl IntoResponse {
    let receiver = state.executor.changes();
    let events = stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(run) => return Some((Ok::<_, Infallible>(run_event(&run)), receiver)),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(events.boxed()).keep_alive(KeepAlive::default())
}
