use crate::http::AppState;
use harness_core::DraftId;
use harness_protocol::{RpcResponse, INTERNAL_ERROR, NOT_FOUND};

fn gc_adopt_task_request(
    prompt: String,
    gc_config: &harness_core::GcConfig,
    project_root: std::path::PathBuf,
) -> crate::task_runner::CreateTaskRequest {
    crate::task_runner::CreateTaskRequest {
        prompt: Some(prompt),
        project: Some(project_root),
        wait_secs: gc_config.adopt_wait_secs,
        max_rounds: gc_config.adopt_max_rounds,
        turn_timeout_secs: gc_config.adopt_turn_timeout_secs,
        max_budget_usd: Some(gc_config.budget_per_signal_usd),
        ..Default::default()
    }
}

pub async fn gc_run(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    let project_root = state.core.project_root.clone();
    let (violations, guard_count) = {
        let rules = state.engines.rules.read().await;
        if let Err(err) = rules.validate_scan_request(None) {
            tracing::warn!(
                project_root = %project_root.display(),
                guard_count = rules.guards().len(),
                error = %err,
                "gc/run rejected before scan"
            );
            return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
        }
        let guard_count = rules.guards().len();
        let violations = match rules.scan(&project_root).await {
            Ok(violations) => violations,
            Err(err) => {
                tracing::warn!(
                    project_root = %project_root.display(),
                    guard_count,
                    error = %err,
                    "gc/run scan failed"
                );
                return RpcResponse::error(id, INTERNAL_ERROR, err.to_string());
            }
        };
        (violations, guard_count)
    };
    tracing::info!(
        project_root = %project_root.display(),
        guard_count,
        violation_count = violations.len(),
        "gc/run scan completed"
    );
    state
        .observability
        .events
        .persist_rule_scan(&project_root, &violations)
        .await;

    let events = match state
        .observability
        .events
        .query(&harness_core::EventFilters::default())
        .await
    {
        Ok(e) => e,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let project = harness_core::Project::from_path(project_root);
    let agent = match state.core.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };
    match state
        .engines
        .gc_agent
        .run(&project, &events, &violations, agent.as_ref(), None)
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
    match state.engines.gc_agent.drafts() {
        Ok(drafts) => RpcResponse::success(id, serde_json::json!({ "draft_count": drafts.len() })),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_drafts(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    match state.engines.gc_agent.drafts() {
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
    let draft = match state.engines.gc_agent.draft_store().get(&draft_id) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return RpcResponse::error(id, NOT_FOUND, format!("draft {} not found", draft_id));
        }
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let artifact_paths: Vec<String> = draft
        .artifacts
        .iter()
        .map(|a| a.target_path.display().to_string())
        .collect();

    match state.engines.gc_agent.adopt(&draft_id) {
        Ok(()) => {
            if artifact_paths.is_empty() || !state.core.server.config.gc.auto_pr {
                return RpcResponse::success(
                    id,
                    serde_json::json!({ "adopted": true, "task_id": null }),
                );
            }
            let task_id = if let Some(agent) = state.core.server.agent_registry.default_agent() {
                let path_refs: Vec<&str> = artifact_paths.iter().map(String::as_str).collect();
                let prompt = harness_core::prompts::gc_adopt_prompt(
                    &draft_id.to_string(),
                    &draft.rationale,
                    &draft.validation,
                    &path_refs,
                );
                let req = gc_adopt_task_request(
                    prompt,
                    &state.core.server.config.gc,
                    state.core.project_root.clone(),
                );
                let project_id = state.core.project_root.to_string_lossy().into_owned();
                let permit = match state.concurrency.task_queue.acquire(&project_id).await {
                    Ok(p) => p,
                    Err(e) => {
                        return RpcResponse::error(
                            id,
                            INTERNAL_ERROR,
                            format!("task queue full: {e}"),
                        );
                    }
                };
                let tid = crate::task_runner::spawn_task(
                    state.core.tasks.clone(),
                    agent,
                    state.core.server.agent_registry.clone(),
                    std::sync::Arc::new(state.core.server.config.clone()),
                    state.engines.skills.clone(),
                    state.observability.events.clone(),
                    state.interceptors.clone(),
                    req,
                    state.concurrency.workspace_mgr.clone(),
                    permit,
                    None,
                    state.concurrency.project_semaphores.clone(),
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

        let req = gc_adopt_task_request(
            "prompt".to_string(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.wait_secs, 7);
        assert_eq!(req.max_rounds, 9);
        assert_eq!(req.turn_timeout_secs, 11);
    }

    #[test]
    fn gc_adopt_task_request_uses_gc_config_defaults() {
        let gc_config = harness_core::GcConfig::default();

        let req = gc_adopt_task_request(
            "prompt".to_string(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.wait_secs, 120);
        assert_eq!(req.max_rounds, 3);
        assert_eq!(req.turn_timeout_secs, 600);
        assert_eq!(req.max_budget_usd, Some(0.5));
    }

    #[test]
    fn gc_adopt_task_request_respects_budget_per_signal() {
        let mut gc_config = harness_core::GcConfig::default();
        gc_config.budget_per_signal_usd = 1.25;

        let req = gc_adopt_task_request(
            "prompt".to_string(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.max_budget_usd, Some(1.25));
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
    match state.engines.gc_agent.reject(&draft_id, reason.as_deref()) {
        Ok(()) => RpcResponse::success(id, serde_json::json!({ "rejected": true })),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}
