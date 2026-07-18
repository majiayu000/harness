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
use futures::{stream::BoxStream, StreamExt};
use harness_core::agent::StreamItem;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::Arc,
    time::Duration,
};

pub(crate) async fn stream_runtime_submission_sse(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.core.workflow_runtime_store.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "workflow runtime store unavailable"})),
        )
            .into_response();
    }
    let task_id = harness_core::types::TaskId(id);
    match runtime_submission_sse_stream(state, task_id).await {
        Ok(Some(stream)) => Sse::new(stream)
            .keep_alive(sse_keep_alive())
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime submission not found"})),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                "stream_runtime_submission_sse: runtime workflow lookup failed: {error}"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

fn sse_keep_alive() -> KeepAlive {
    KeepAlive::new()
        .interval(Duration::from_secs(30))
        .text("heartbeat")
}

type SseEvent = Result<Event, Infallible>;

async fn runtime_submission_sse_stream(
    state: Arc<AppState>,
    task_id: harness_core::types::TaskId,
) -> anyhow::Result<Option<BoxStream<'static, SseEvent>>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref().cloned() else {
        return Ok(None);
    };
    let Some(workflow) =
        crate::workflow_runtime_submission::runtime_issue_by_submission_id(&store, &task_id)
            .await?
    else {
        return Ok(None);
    };

    let mut cursor = RuntimeSubmissionSseCursor::new(store, workflow.id);
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
                        "runtime_submission_sse_stream: workflow refresh failed: {error}"
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

struct RuntimeSubmissionSseCursor {
    store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
    workflow_id: String,
    pending: VecDeque<SseEvent>,
    last_state: Option<String>,
    last_event_sequence: u64,
    command_statuses: HashMap<String, String>,
    finished: bool,
}

impl RuntimeSubmissionSseCursor {
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
                .is_none_or(|status| status != command.status.as_str());
            if changed {
                self.push_text(format!(
                    "[command] {} status={} activity={}",
                    command.id,
                    command.status,
                    command.command.runtime_activity_key()
                ));
                self.command_statuses
                    .insert(command.id.clone(), command.status.to_string());
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
                tracing::warn!("runtime_submission_sse_stream: failed to serialize event: {error}")
            }
        }
    }
}
