use crate::http::AppState;
use harness_core::ThreadId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};
use std::path::PathBuf;

/// Persist an existing thread to the optional ThreadDb after a mutation.
pub(crate) async fn persist_thread(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.thread_db {
        if let Some(thread) = state.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.update(&thread).await {
                tracing::warn!("thread_db persist failed: {e}");
            }
        }
    }
}

/// Insert a newly created thread into the optional ThreadDb.
pub(crate) async fn persist_thread_insert(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.thread_db {
        if let Some(thread) = state.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.insert(&thread).await {
                tracing::warn!("thread_db insert failed: {e}");
            }
        }
    }
}

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
            }
        }),
    )
}

pub async fn thread_start(
    state: &AppState,
    id: Option<serde_json::Value>,
    cwd: PathBuf,
) -> RpcResponse {
    let cwd = match crate::handlers::validate_project_root(&cwd) {
        Ok(p) => p,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e),
    };
    let thread_id = state.server.thread_manager.start_thread(cwd);
    persist_thread_insert(state, &thread_id).await;
    RpcResponse::success(id, serde_json::json!({ "thread_id": thread_id }))
}

pub async fn thread_list(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    let threads = state.server.thread_manager.list_threads();
    RpcResponse::success(id, serde_json::to_value(threads).unwrap_or_default())
}

pub async fn thread_delete(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    let deleted = state.server.thread_manager.delete_thread(&thread_id);
    if deleted {
        if let Some(db) = &state.thread_db {
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
    let input = crate::handlers::sanitize_user_input(&input);
    let agent_id =
        harness_core::AgentId::from_str(&state.server.config.agents.default_agent);
    match state
        .server
        .thread_manager
        .start_turn(&thread_id, input, agent_id)
    {
        Ok(turn_id) => {
            persist_thread(state, &thread_id).await;
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
    match state.server.thread_manager.find_thread_for_turn(&turn_id) {
        Some(thread_id) => {
            match state
                .server
                .thread_manager
                .cancel_turn(&thread_id, &turn_id)
            {
                Ok(()) => {
                    persist_thread(state, &thread_id).await;
                    RpcResponse::success(id, serde_json::json!({ "cancelled": true }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
    }
}

pub async fn turn_status(
    state: &AppState,
    id: Option<serde_json::Value>,
    turn_id: harness_core::TurnId,
) -> RpcResponse {
    match state.server.thread_manager.find_thread_for_turn(&turn_id) {
        Some(thread_id) => {
            if let Some(thread) = state.server.thread_manager.get_thread(&thread_id) {
                if let Some(turn) = thread.turns.iter().find(|t| t.id == turn_id) {
                    match serde_json::to_value(turn) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                } else {
                    RpcResponse::error(id, INTERNAL_ERROR, "turn not found")
                }
            } else {
                RpcResponse::error(id, INTERNAL_ERROR, "thread not found")
            }
        }
        None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
    }
}

pub async fn turn_steer(
    state: &AppState,
    id: Option<serde_json::Value>,
    turn_id: harness_core::TurnId,
    instruction: String,
) -> RpcResponse {
    let instruction = crate::handlers::sanitize_user_input(&instruction);
    match state.server.thread_manager.find_thread_for_turn(&turn_id) {
        Some(thread_id) => {
            match state
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
        None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
    }
}

pub async fn thread_resume(
    state: &AppState,
    id: Option<serde_json::Value>,
    thread_id: ThreadId,
) -> RpcResponse {
    match state.server.thread_manager.resume_thread(&thread_id) {
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
    match state.server.thread_manager.compact_thread(&thread_id) {
        Ok(()) => {
            persist_thread(state, &thread_id).await;
            RpcResponse::success(id, serde_json::json!({ "compacted": true }))
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
