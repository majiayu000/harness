use crate::http::AppState;
use harness_core::DraftId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};

pub async fn gc_run(
    state: &AppState,
    id: Option<serde_json::Value>,
) -> RpcResponse {
    let events = match state.events.query(&harness_core::EventFilters::default()) {
        Ok(e) => e,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let project_root = std::path::PathBuf::from(".");
    let violations = {
        let rules = state.rules.read().await;
        rules.scan(&project_root).await.unwrap_or_default()
    };
    let project = harness_core::Project::from_path(project_root);
    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };
    match state
        .gc_agent
        .run(&project, &events, &violations, agent.as_ref())
        .await
    {
        Ok(report) => match serde_json::to_value(&report) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_status(
    state: &AppState,
    id: Option<serde_json::Value>,
) -> RpcResponse {
    match state.gc_agent.drafts() {
        Ok(drafts) => RpcResponse::success(
            id,
            serde_json::json!({ "draft_count": drafts.len() }),
        ),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_drafts(
    state: &AppState,
    id: Option<serde_json::Value>,
) -> RpcResponse {
    match state.gc_agent.drafts() {
        Ok(drafts) => match serde_json::to_value(&drafts) {
            Ok(v) => RpcResponse::success(id, v),
            Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
        },
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_adopt(
    state: &AppState,
    id: Option<serde_json::Value>,
    draft_id: DraftId,
) -> RpcResponse {
    let draft = match state.gc_agent.draft_store().get(&draft_id) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return RpcResponse::error(
                id,
                INTERNAL_ERROR,
                format!("draft {} not found", draft_id),
            );
        }
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let artifact_paths: Vec<String> = draft
        .artifacts
        .iter()
        .map(|a| a.target_path.display().to_string())
        .collect();

    match state.gc_agent.adopt(&draft_id) {
        Ok(()) => {
            if artifact_paths.is_empty() {
                return RpcResponse::success(
                    id,
                    serde_json::json!({ "adopted": true, "task_id": null }),
                );
            }
            const GC_ADOPT_WAIT_SECS: u64 = 120;
            const GC_ADOPT_MAX_ROUNDS: u32 = 3;
            const GC_ADOPT_TURN_TIMEOUT_SECS: u64 = 600;
            let task_id =
                if let Some(agent) = state.server.agent_registry.default_agent() {
                    let paths_list = artifact_paths.join(", ");
                    let safe_paths = harness_core::prompts::wrap_external_data(&paths_list);
                    let prompt = format!(
                        "GC drafted the following files:\n{safe_paths}\n\
                         Review these changes, create a branch named gc/{draft_id}, \
                         commit, push, and open a PR. \
                         Print PR_URL=<url> on the last line."
                    );
                    let req = crate::task_runner::CreateTaskRequest {
                        prompt: Some(prompt),
                        issue: None,
                        pr: None,
                        project: None,
                        wait_secs: GC_ADOPT_WAIT_SECS,
                        max_rounds: GC_ADOPT_MAX_ROUNDS,
                        turn_timeout_secs: GC_ADOPT_TURN_TIMEOUT_SECS,
                    };
                    let tid = crate::task_runner::spawn_task(
                        state.tasks.clone(),
                        agent,
                        state.skills.clone(),
                        state.events.clone(),
                        state.interceptors.clone(),
                        req,
                    )
                    .await;
                    Some(tid.0)
                } else {
                    None
                };
            RpcResponse::success(
                id,
                serde_json::json!({ "adopted": true, "task_id": task_id }),
            )
        }
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_reject(
    state: &AppState,
    id: Option<serde_json::Value>,
    draft_id: DraftId,
    reason: Option<String>,
) -> RpcResponse {
    match state.gc_agent.reject(&draft_id, reason.as_deref()) {
        Ok(()) => RpcResponse::success(id, serde_json::json!({ "rejected": true })),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
