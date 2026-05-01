use super::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use harness_core::agent::StreamItem;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::Arc,
    time::Duration,
};

use futures::{stream::BoxStream, StreamExt};

/// GET /tasks/{id}/stream — real-time SSE stream of agent execution events.
///
/// For active tasks, subscribes to the broadcast channel and forwards each
/// [`StreamItem`] as a JSON-encoded SSE data event.
///
/// For completed tasks (stream channel already closed), replays stored round
/// history as synthetic [`StreamItem::MessageDelta`] events followed by
/// [`StreamItem::Done`], so the Logs view is never blank.
///
/// If the receiver lags on a live stream, a synthetic "lag" event notes how
/// many events were dropped; streaming then continues.
pub(crate) async fn stream_task_sse(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);

    let rx = match state.core.tasks.subscribe_task_stream(&task_id) {
        Some(rx) => rx,
        None => {
            match state.core.tasks.get_with_db_fallback(&task_id).await {
                Ok(None) => match runtime_task_sse_stream(state.clone(), task_id.clone()).await {
                    Ok(Some(stream)) => {
                        return Sse::new(stream)
                            .keep_alive(sse_keep_alive())
                            .into_response();
                    }
                    Ok(None) => {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error": "task not found"})),
                        )
                            .into_response();
                    }
                    Err(e) => {
                        tracing::error!("stream_task_sse: runtime workflow lookup failed: {e}");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "internal server error"})),
                        )
                            .into_response();
                    }
                },
                Err(e) => {
                    tracing::error!("stream_task_sse: database error: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
                Ok(Some(task)) => {
                    // Task exists but stream already closed — replay stored
                    // round history so the Logs view is not blank.
                    let mut events: Vec<Result<Event, std::convert::Infallible>> = task
                        .rounds
                        .iter()
                        .filter_map(|r| {
                            let mut text = format!("[turn {}] {}\n{}", r.turn, r.action, r.result);
                            if let Some(detail) = &r.detail {
                                text.push_str("\n\n");
                                text.push_str(detail);
                            }
                            if let Some(telemetry) = &r.telemetry {
                                text.push_str("\n\ntelemetry=");
                                text.push_str(&serde_json::to_string(telemetry).ok()?);
                            }
                            if let Some(failure) = &r.failure {
                                text.push_str("\nfailure=");
                                text.push_str(&serde_json::to_string(failure).ok()?);
                            }
                            let item = StreamItem::MessageDelta { text };
                            serde_json::to_string(&item)
                                .ok()
                                .map(|data| Ok(Event::default().data(data)))
                        })
                        .collect();
                    if let Ok(data) = serde_json::to_string(&StreamItem::Done) {
                        events.push(Ok(Event::default().data(data)));
                    }
                    return Sse::new(futures::stream::iter(events)).into_response();
                }
            }
        }
    };

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(item) => {
                let data = match serde_json::to_string(&item) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("sse: failed to serialize event: {e}");
                        String::new()
                    }
                };
                Some((
                    Ok::<Event, std::convert::Infallible>(Event::default().data(data)),
                    rx,
                ))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let event = Event::default()
                    .event("lag")
                    .data(format!("dropped {n} events due to slow consumer"));
                Some((Ok(event), rx))
            }
        }
    });

    // Send a heartbeat comment every 30 s so reverse proxies (nginx default
    // 60 s idle timeout) don't drop the connection while the agent is silent.
    Sse::new(stream)
        .keep_alive(sse_keep_alive())
        .into_response()
}

fn sse_keep_alive() -> KeepAlive {
    KeepAlive::new()
        .interval(Duration::from_secs(30))
        .text("heartbeat")
}

type SseEvent = Result<Event, Infallible>;

async fn runtime_task_sse_stream(
    state: Arc<AppState>,
    task_id: harness_core::types::TaskId,
) -> anyhow::Result<Option<BoxStream<'static, SseEvent>>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref().cloned() else {
        return Ok(None);
    };
    let Some(workflow) =
        crate::workflow_runtime_submission::runtime_issue_by_task_id(&store, &task_id).await?
    else {
        return Ok(None);
    };

    let mut cursor = RuntimeTaskSseCursor::new(store, workflow.id);
    cursor.refresh().await?;
    Ok(Some(
        futures::stream::unfold(cursor, |mut cursor| async move {
            loop {
                if let Some(event) = cursor.pending.pop_front() {
                    return Some((event, cursor));
                }
                if cursor.finished {
                    return None;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(error) = cursor.refresh().await {
                    tracing::error!(
                        "runtime_task_sse_stream: runtime workflow refresh failed: {error}"
                    );
                    cursor.push_item(StreamItem::Error {
                        message: "runtime workflow stream failed".to_string(),
                    });
                    cursor.finished = true;
                }
            }
        })
        .boxed(),
    ))
}

struct RuntimeTaskSseCursor {
    store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
    workflow_id: String,
    pending: VecDeque<SseEvent>,
    last_state: Option<String>,
    last_event_sequence: u64,
    command_statuses: HashMap<String, String>,
    finished: bool,
}

impl RuntimeTaskSseCursor {
    fn new(
        store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
        workflow_id: String,
    ) -> Self {
        Self {
            store,
            workflow_id,
            pending: VecDeque::new(),
            last_state: None,
            last_event_sequence: 0,
            command_statuses: HashMap::new(),
            finished: false,
        }
    }

    async fn refresh(&mut self) -> anyhow::Result<()> {
        let Some(workflow) = self.store.get_instance(&self.workflow_id).await? else {
            self.push_item(StreamItem::Error {
                message: "runtime workflow not found".to_string(),
            });
            self.finished = true;
            return Ok(());
        };

        if self.last_state.as_deref() != Some(workflow.state.as_str()) {
            self.push_text(format!(
                "[workflow] {} state={}",
                workflow.id, workflow.state
            ));
            self.last_state = Some(workflow.state.clone());
        }

        for event in self.store.events_for(&workflow.id).await? {
            if event.sequence <= self.last_event_sequence {
                continue;
            }
            self.push_text(format!(
                "[event {}] {} {}",
                event.sequence, event.event_type, event.event
            ));
            self.last_event_sequence = event.sequence;
        }

        for command in self.store.commands_for(&workflow.id).await? {
            let changed = self
                .command_statuses
                .get(&command.id)
                .is_none_or(|status| status != &command.status);
            if changed {
                self.push_text(format!(
                    "[command] {} status={} activity={}",
                    command.id,
                    command.status,
                    command.command.runtime_activity_key()
                ));
                self.command_statuses
                    .insert(command.id.clone(), command.status.clone());
            }
        }

        if workflow.is_terminal() {
            self.push_item(StreamItem::Done);
            self.finished = true;
        }
        Ok(())
    }

    fn push_text(&mut self, text: String) {
        self.push_item(StreamItem::MessageDelta { text });
    }

    fn push_item(&mut self, item: StreamItem) {
        match serde_json::to_string(&item) {
            Ok(data) => self.pending.push_back(Ok(Event::default().data(data))),
            Err(error) => {
                tracing::warn!("runtime_task_sse_stream: failed to serialize event: {error}")
            }
        }
    }
}
