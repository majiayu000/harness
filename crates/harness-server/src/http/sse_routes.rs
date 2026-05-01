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
use std::sync::Arc;

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
                Ok(None) => match runtime_task_sse_replay(&state, &task_id).await {
                    Ok(Some(events)) => {
                        return Sse::new(futures::stream::iter(events)).into_response();
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
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(30))
                .text("heartbeat"),
        )
        .into_response()
}

async fn runtime_task_sse_replay(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<Vec<Result<Event, std::convert::Infallible>>>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) =
        crate::workflow_runtime_submission::runtime_issue_by_task_id(store, task_id).await?
    else {
        return Ok(None);
    };
    let mut lines = Vec::new();
    lines.push(format!(
        "[workflow] {} state={}",
        workflow.id, workflow.state
    ));
    for event in store.events_for(&workflow.id).await? {
        lines.push(format!(
            "[event {}] {} {}",
            event.sequence, event.event_type, event.event
        ));
    }
    for command in store.commands_for(&workflow.id).await? {
        lines.push(format!(
            "[command] {} status={} activity={}",
            command.id,
            command.status,
            command.command.runtime_activity_key()
        ));
    }
    let mut events: Vec<Result<Event, std::convert::Infallible>> = lines
        .into_iter()
        .filter_map(|text| {
            serde_json::to_string(&StreamItem::MessageDelta { text })
                .ok()
                .map(|data| Ok(Event::default().data(data)))
        })
        .collect();
    if let Ok(data) = serde_json::to_string(&StreamItem::Done) {
        events.push(Ok(Event::default().data(data)));
    }
    Ok(Some(events))
}
