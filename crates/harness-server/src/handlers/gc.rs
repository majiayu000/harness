use crate::http::AppState;
use harness_core::DraftId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR};

fn gc_adopt_task_request(
    prompt: String,
    gc_config: &harness_core::GcConfig,
    project_root: std::path::PathBuf,
) -> crate::task_runner::CreateTaskRequest {
    crate::task_runner::CreateTaskRequest {
        prompt: Some(prompt),
        issue: None,
        pr: None,
        project: Some(project_root),
        wait_secs: gc_config.adopt_wait_secs,
        max_rounds: gc_config.adopt_max_rounds,
        turn_timeout_secs: gc_config.adopt_turn_timeout_secs,
    }
}

pub async fn gc_run(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    let project_root = state.project_root.clone();
    let violations = {
        let rules = state.rules.read().await;
        rules.scan(&project_root).await.unwrap_or_default()
    };
    crate::handlers::persist_violations(&state.events, &project_root, &violations);

    let events = match state.events.query(&harness_core::EventFilters::default()) {
        Ok(e) => e,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
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

pub async fn gc_status(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    match state.gc_agent.drafts() {
        Ok(drafts) => RpcResponse::success(id, serde_json::json!({ "draft_count": drafts.len() })),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_drafts(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
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
            return RpcResponse::error(id, INTERNAL_ERROR, format!("draft {} not found", draft_id));
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
            let task_id = if let Some(agent) = state.server.agent_registry.default_agent() {
                let paths_list = artifact_paths.join(", ");
                let safe_paths = harness_core::prompts::wrap_external_data(&paths_list);
                let prompt = format!(
                    "GC drafted the following files:\n{safe_paths}\n\
                         Review these changes, create a branch named gc/{draft_id}, \
                         commit, push, and open a PR. \
                         Print PR_URL=<url> on the last line."
                );
                let req = gc_adopt_task_request(prompt, &state.server.config.gc, state.project_root.clone());
                let (reviewer, review_config) = crate::http::resolve_reviewer(
                    &state.server.agent_registry,
                    &state.server.config.agents.review,
                    agent.name(),
                );
                let tid = crate::task_runner::spawn_task(
                    state.tasks.clone(),
                    agent,
                    reviewer,
                    review_config,
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

#[cfg(test)]
mod tests {
    use super::gc_adopt_task_request;

    #[test]
    fn gc_adopt_task_request_uses_gc_config_values() {
        let mut gc_config = harness_core::GcConfig::default();
        gc_config.adopt_wait_secs = 7;
        gc_config.adopt_max_rounds = 9;
        gc_config.adopt_turn_timeout_secs = 11;

        let req = gc_adopt_task_request("prompt".to_string(), &gc_config, std::path::PathBuf::from("/tmp/project"));

        assert_eq!(req.wait_secs, 7);
        assert_eq!(req.max_rounds, 9);
        assert_eq!(req.turn_timeout_secs, 11);
    }

    #[test]
    fn gc_adopt_task_request_uses_gc_config_defaults() {
        let gc_config = harness_core::GcConfig::default();

        let req = gc_adopt_task_request("prompt".to_string(), &gc_config, std::path::PathBuf::from("/tmp/project"));

        assert_eq!(req.wait_secs, 120);
        assert_eq!(req.max_rounds, 3);
        assert_eq!(req.turn_timeout_secs, 600);
    }

    #[test]
    fn gc_adopt_task_request_sets_project_root() {
        let gc_config = harness_core::GcConfig::default();
        let project_root = std::path::PathBuf::from("/tmp/my-project");

        let req = gc_adopt_task_request("prompt".to_string(), &gc_config, project_root.clone());

        assert_eq!(req.project, Some(project_root));
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
