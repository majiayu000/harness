use crate::{http::AppState, validate_root};
use harness_core::{ThreadId, ThreadStatus, TurnStatus};
use harness_protocol::{Notification, RpcResponse, INTERNAL_ERROR, NOT_FOUND};
use std::path::PathBuf;

/// Persist an existing thread to the optional ThreadDb after a mutation.
pub(crate) async fn persist_thread(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.core.thread_db {
        if let Some(thread) = state.core.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.update(&thread).await {
                tracing::warn!("thread_db persist failed: {e}");
            }
        }
    }
}

/// Insert a newly created thread into the optional ThreadDb.
pub(crate) async fn persist_thread_insert(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.core.thread_db {
        if let Some(thread) = state.core.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.insert(&thread).await {
                tracing::warn!("thread_db insert failed: {e}");
            }
        }
    }
}

/// Handle the `initialized` notification sent by the client after `initialize`.
///
/// Per JSON-RPC 2.0, notifications do not get a response. The HTTP layer will
/// return `204 No Content` when the incoming request has no `id`.
pub async fn initialized() {}

pub async fn initialize(id: Option<serde_json::Value>) -> RpcResponse {
    RpcResponse::success(
        id,
        serde_json::json!({
            "name": "harness",
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": {
                "threads": true,
                "gc": true,
                "skills": true,
                "rules": true,
                "exec_plan": true,
                "observe": true,
                "notifications": true,
            }
        }),
    )
}

pub async fn thread_start(
    state: &AppState,
    id: Option<serde_json::Value>,
    cwd: PathBuf,
) -> RpcResponse {
    let cwd = validate_root!(&cwd, id);
    let thread_id = state.core.server.thread_manager.start_thread(cwd);
    persist_thread_insert(state, &thread_id).await;
    crate::notify::emit(
        &state.notifications.notify_tx,
        Notification::ThreadStatusChanged {
            thread_id: thread_id.clone(),
            status: ThreadStatus::Idle,
        },
    );
    RpcResponse::success(id, serde_json::json!({ "thread_id": thread_id }))
}

pub async fn thread_list(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    let threads = state.core.server.thread_manager.list_threads();
    thread_list_response(id, threads)
}

fn thread_list_response<T: serde::Serialize>(
    id: Option<serde_json::Value>,
    threads: T,
) -> RpcResponse {
    match serde_json::to_value(threads) {
        Ok(value) => RpcResponse::success(id, value),
        Err(e) => RpcResponse::error(
            id,
            INTERNAL_ERROR,
            format!("failed to serialize thread list: {e}"),
        ),
    }
}

pub async fn thread_delete(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    let deleted = state.core.server.thread_manager.delete_thread(&thread_id);
    if deleted {
        if let Some(db) = &state.core.thread_db {
            if let Err(e) = db.delete(thread_id.as_str()).await {
                tracing::warn!("thread_db delete failed: {e}");
            }
        }
    }
    RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
}

pub async fn turn_start(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
    input: String,
) -> RpcResponse {
    let input = harness_core::prompts::wrap_external_data(&input);
    let agent_name = state.core.server.config.agents.default_agent.clone();
    let agent_id = harness_core::AgentId::from_str(&agent_name);
    match state
        .core
        .server
        .thread_manager
        .start_turn(&thread_id, input.clone(), agent_id)
    {
        Ok(turn_id) => {
            persist_thread(state, &thread_id).await;
            crate::notify::emit(
                &state.notifications.notify_tx,
                Notification::TurnStarted {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                },
            );

            let lifecycle_server = state.core.server.clone();
            let cleanup_server = state.core.server.clone();
            let lifecycle_thread_db = state.core.thread_db.clone();
            let lifecycle_notify_tx = state.notifications.notify_tx.clone();
            let lifecycle_notification_tx = state.notifications.notification_tx.clone();
            let lifecycle_thread_id = thread_id.clone();
            let lifecycle_turn_id = turn_id.clone();
            let cleanup_turn_id = turn_id.clone();
            let lifecycle_prompt = input;
            let lifecycle_agent = agent_name;

            let handle = tokio::spawn(async move {
                crate::task_executor::run_turn_lifecycle(
                    lifecycle_server,
                    lifecycle_thread_db,
                    lifecycle_notify_tx,
                    lifecycle_notification_tx,
                    lifecycle_thread_id,
                    lifecycle_turn_id,
                    lifecycle_prompt,
                    lifecycle_agent,
                )
                .await;
                cleanup_server
                    .thread_manager
                    .clear_turn_task(&cleanup_turn_id);
            });
            state
                .core
                .server
                .thread_manager
                .register_turn_task(&turn_id, handle);

            RpcResponse::success(id, serde_json::json!({ "turn_id": turn_id }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn turn_cancel(
    state: &AppState,
    id: Option<serde_json::Value>,
    turn_id: harness_core::TurnId,
) -> RpcResponse {
    match state
        .core
        .server
        .thread_manager
        .find_thread_for_turn(&turn_id)
    {
        Some(thread_id) => {
            match state
                .core
                .server
                .thread_manager
                .cancel_turn(&thread_id, &turn_id)
            {
                Ok(cancel_usage) => {
                    let cancelled = cancel_usage.is_some();
                    persist_thread(state, &thread_id).await;
                    if let Some(token_usage) = cancel_usage {
                        crate::notify::emit(
                            &state.notifications.notify_tx,
                            Notification::TurnCompleted {
                                turn_id: turn_id.clone(),
                                status: TurnStatus::Cancelled,
                                token_usage,
                            },
                        );
                    }
                    RpcResponse::success(id, serde_json::json!({ "cancelled": cancelled }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        None => RpcResponse::error(id, NOT_FOUND, "turn not found in any thread"),
    }
}

pub async fn turn_status(
    state: &AppState,
    id: Option<serde_json::Value>,
    turn_id: harness_core::TurnId,
) -> RpcResponse {
    match state
        .core
        .server
        .thread_manager
        .find_thread_for_turn(&turn_id)
    {
        Some(thread_id) => {
            if let Some(thread) = state.core.server.thread_manager.get_thread(&thread_id) {
                if let Some(turn) = thread.turns.iter().find(|t| t.id == turn_id) {
                    match serde_json::to_value(turn) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                } else {
                    RpcResponse::error(id, NOT_FOUND, "turn not found")
                }
            } else {
                RpcResponse::error(id, NOT_FOUND, "thread not found")
            }
        }
        None => RpcResponse::error(id, NOT_FOUND, "turn not found in any thread"),
    }
}

pub async fn turn_steer(
    state: &AppState,
    id: Option<serde_json::Value>,
    turn_id: harness_core::TurnId,
    instruction: String,
) -> RpcResponse {
    let instruction = harness_core::prompts::wrap_external_data(&instruction);
    match state
        .core
        .server
        .thread_manager
        .find_thread_for_turn(&turn_id)
    {
        Some(thread_id) => {
            match state
                .core
                .server
                .thread_manager
                .steer_turn(&thread_id, &turn_id, instruction)
            {
                Ok(()) => {
                    persist_thread(state, &thread_id).await;
                    RpcResponse::success(id, serde_json::json!({ "steered": true }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        None => RpcResponse::error(id, NOT_FOUND, "turn not found in any thread"),
    }
}

pub async fn thread_resume(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    match state.core.server.thread_manager.resume_thread(&thread_id) {
        Ok(()) => {
            persist_thread(state, &thread_id).await;
            RpcResponse::success(id, serde_json::json!({ "resumed": true }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn thread_fork(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
    from_turn: Option<harness_core::TurnId>,
) -> RpcResponse {
    match state
        .core
        .server
        .thread_manager
        .fork_thread(&thread_id, from_turn.as_ref())
    {
        Ok(new_id) => {
            persist_thread_insert(state, &new_id).await;
            RpcResponse::success(id, serde_json::json!({ "thread_id": new_id }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn thread_compact(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    match state.core.server.thread_manager.compact_thread(&thread_id) {
        Ok(()) => {
            persist_thread(state, &thread_id).await;
            RpcResponse::success(id, serde_json::json!({ "compacted": true }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use serde_json::json;

    struct AlwaysFailSerialize;

    impl serde::Serialize for AlwaysFailSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("forced serialization failure"))
        }
    }

    #[test]
    fn thread_list_response_returns_success_when_serialization_succeeds() {
        let response = thread_list_response(Some(json!(1)), vec![json!({ "thread_id": "t-1" })]);

        assert_eq!(response.result, Some(json!([{ "thread_id": "t-1" }])));
        assert!(response.error.is_none());
    }

    #[test]
    fn thread_list_response_returns_error_when_serialization_fails() {
        let response = thread_list_response(Some(json!(1)), AlwaysFailSerialize);

        assert!(response.result.is_none());
        let error = response
            .error
            .expect("expected serialization error response");
        assert_eq!(error.code, INTERNAL_ERROR);
        assert!(error
            .message
            .contains("failed to serialize thread list: forced serialization failure"));
    }
}
