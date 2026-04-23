use crate::http::{resolve_reviewer, AppState};
use harness_core::{
    config::resolve::resolve_config,
    types::{Decision, DraftId, DraftStatus, Event, ProjectId, SessionId},
};
use harness_protocol::{
    methods::RpcResponse, methods::CONFLICT, methods::INTERNAL_ERROR, methods::NOT_FOUND,
};
use std::path::Path;

fn gc_adopt_task_request(
    prompt: String,
    gc_config: &harness_core::config::misc::GcConfig,
    project_root: std::path::PathBuf,
) -> crate::task_runner::CreateTaskRequest {
    crate::task_runner::CreateTaskRequest {
        prompt: Some(prompt),
        project: Some(project_root),
        wait_secs: gc_config.adopt_wait_secs,
        max_rounds: Some(gc_config.adopt_max_rounds),
        turn_timeout_secs: gc_config.adopt_turn_timeout_secs,
        max_budget_usd: Some(gc_config.budget_per_signal_usd),
        ..Default::default()
    }
}

fn configured_project_id(project_root: &Path) -> ProjectId {
    ProjectId::from_path(project_root)
}

async fn log_gc_event(
    state: &AppState,
    hook: &str,
    decision: Decision,
    reason: Option<String>,
    detail: Option<String>,
) {
    let mut event = Event::new(SessionId::new(), hook, "gc_handler", decision);
    event.reason = reason;
    event.detail = detail;
    if let Err(e) = state.observability.events.log(&event).await {
        tracing::warn!(hook, error = %e, "failed to log gc event");
    }
}

pub async fn gc_run(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_id: Option<ProjectId>,
) -> RpcResponse {
    let project_root = state.core.project_root.clone();
    let configured_project = configured_project_id(&project_root);
    if let Some(requested) = project_id {
        if requested != configured_project {
            return RpcResponse::error(
                id,
                NOT_FOUND,
                format!(
                    "project '{}' not available on this server instance",
                    requested.as_str()
                ),
            );
        }
    }

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
        .query(&harness_core::types::EventFilters::default())
        .await
    {
        Ok(e) => e,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let external_signals = match state.observability.events.query_external_signals(None) {
        Ok(s) => s,
        Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    };
    let project = harness_core::types::Project {
        id: configured_project,
        root: project_root.clone(),
        languages: Vec::new(),
        name: project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string(),
    };
    let agent = match state.core.server.agent_registry.default_agent() {
        Some(a) => a,
        None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
    };
    match state
        .engines
        .gc_agent
        .run_with_external(
            &project,
            &events,
            &violations,
            &external_signals,
            agent.as_ref(),
        )
        .await
    {
        Ok(report) => {
            log_gc_event(
                state,
                "gc_run",
                if report.errors.is_empty() {
                    Decision::Complete
                } else {
                    Decision::Warn
                },
                Some(format!(
                    "signals={} drafts={} errors={} external={}",
                    report.signals.len(),
                    report.drafts_generated,
                    report.errors.len(),
                    external_signals.len()
                )),
                Some(project_root.display().to_string()),
            )
            .await;
            match serde_json::to_value(&report) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
        Err(e) => {
            log_gc_event(
                state,
                "gc_run",
                Decision::Block,
                Some(format!("gc_agent_run_failed: {e}")),
                Some(project_root.display().to_string()),
            )
            .await;
            RpcResponse::error(id, INTERNAL_ERROR, e.to_string())
        }
    }
}

pub async fn gc_status(state: &AppState, id: Option<serde_json::Value>) -> RpcResponse {
    match state.engines.gc_agent.drafts() {
        Ok(drafts) => RpcResponse::success(id, serde_json::json!({ "draft_count": drafts.len() })),
        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

pub async fn gc_drafts(
    state: &AppState,
    id: Option<serde_json::Value>,
    project_id: Option<ProjectId>,
) -> RpcResponse {
    let configured_project = configured_project_id(&state.core.project_root);
    if let Some(requested) = project_id.clone() {
        if requested != configured_project {
            return RpcResponse::error(
                id,
                NOT_FOUND,
                format!(
                    "project '{}' not available on this server instance",
                    requested.as_str()
                ),
            );
        }
    }
    match state.engines.gc_agent.drafts() {
        Ok(drafts) => {
            let filtered = if let Some(pid) = project_id {
                drafts
                    .into_iter()
                    .filter(|draft| draft.signal.project_id == pid)
                    .collect::<Vec<_>>()
            } else {
                drafts
            };
            match serde_json::to_value(&filtered) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }
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
    if draft.status != DraftStatus::Pending {
        return RpcResponse::error(
            id,
            CONFLICT,
            format!(
                "cannot adopt draft {}: status is {:?}, expected Pending",
                draft_id, draft.status
            ),
        );
    }
    let artifact_paths: Vec<String> = draft
        .artifacts
        .iter()
        .map(|a| a.target_path.display().to_string())
        .collect();
    let needs_task_dispatch = !artifact_paths.is_empty() && state.core.server.config.gc.auto_pr;

    let task_dispatch_plan = if needs_task_dispatch {
        let Some(agent) = state.core.server.agent_registry.default_agent() else {
            return RpcResponse::error(
                id,
                INTERNAL_ERROR,
                "gc_adopt auto_pr requires a registered default agent",
            );
        };
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
        let project_config =
            match harness_core::config::project::load_project_config(&state.core.project_root) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return RpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("failed to load project config: {e}"),
                    );
                }
            };
        let resolved = resolve_config(&state.core.server.config, &project_config);
        let (reviewer, _review_config) = resolve_reviewer(
            &state.core.server.agent_registry,
            &resolved.review,
            agent.name(),
        );
        Some((
            agent,
            reviewer,
            req,
            state.core.project_root.to_string_lossy().into_owned(),
        ))
    } else {
        None
    };

    match state.engines.gc_agent.adopt(&draft_id) {
        Ok(()) => {
            if !needs_task_dispatch {
                log_gc_event(
                    state,
                    "gc_adopt",
                    Decision::Complete,
                    Some(format!("draft={} task_id=none", draft_id)),
                    Some("adopted_without_pr".to_string()),
                )
                .await;
                return RpcResponse::success(
                    id,
                    serde_json::json!({ "adopted": true, "task_id": null }),
                );
            }
            let dispatch_result =
                if let Some((agent, reviewer, req, project_id)) = task_dispatch_plan {
                    match state.concurrency.task_queue.acquire(&project_id, 0).await {
                        Ok(permit) => {
                            let tid = crate::task_runner::spawn_task(
                                state.core.tasks.clone(),
                                agent,
                                reviewer,
                                std::sync::Arc::new(state.core.server.config.clone()),
                                state.engines.skills.clone(),
                                state.observability.events.clone(),
                                state.interceptors.clone(),
                                req,
                                state.concurrency.workspace_mgr.clone(),
                                permit,
                                None,
                                state.core.issue_workflow_store.clone(),
                            )
                            .await;
                            Ok(Some(tid.0))
                        }
                        Err(e) => Err(format!("task queue full: {e}")),
                    }
                } else {
                    Ok(None)
                };

            let (task_id, dispatch_error) = match dispatch_result {
                Ok(task_id) => (task_id, None),
                Err(err) => (None, Some(err)),
            };
            let decision = if dispatch_error.is_some() {
                Decision::Warn
            } else {
                Decision::Complete
            };
            log_gc_event(
                state,
                "gc_adopt",
                decision,
                Some(format!(
                    "draft={} task_id={}{}",
                    draft_id,
                    task_id.as_deref().unwrap_or("none"),
                    dispatch_error
                        .as_ref()
                        .map(|e| format!(" dispatch_error={e}"))
                        .unwrap_or_default()
                )),
                None,
            )
            .await;
            RpcResponse::success(
                id,
                serde_json::json!({
                    "adopted": true,
                    "task_id": task_id,
                    "task_dispatch_error": dispatch_error
                }),
            )
        }
        Err(e) => {
            // Re-read to distinguish a concurrent state-conflict from a true internal error.
            let error_code = state
                .engines
                .gc_agent
                .draft_store()
                .get(&draft_id)
                .ok()
                .flatten()
                .map(|d| {
                    if d.status != DraftStatus::Pending {
                        CONFLICT
                    } else {
                        INTERNAL_ERROR
                    }
                })
                .unwrap_or(INTERNAL_ERROR);
            log_gc_event(
                state,
                "gc_adopt",
                Decision::Block,
                Some(format!("draft={} error={e}", draft_id)),
                None,
            )
            .await;
            RpcResponse::error(id, error_code, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gc_adopt_task_request;

    #[test]
    fn gc_adopt_task_request_uses_gc_config_values() {
        let mut gc_config = harness_core::config::misc::GcConfig::default();
        gc_config.adopt_wait_secs = 7;
        gc_config.adopt_max_rounds = 9;
        gc_config.adopt_turn_timeout_secs = 11;

        let req = gc_adopt_task_request(
            "prompt".to_string(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.wait_secs, 7);
        assert_eq!(req.max_rounds, Some(9));
        assert_eq!(req.turn_timeout_secs, 11);
    }

    #[test]
    fn gc_adopt_task_request_uses_gc_config_defaults() {
        let gc_config = harness_core::config::misc::GcConfig::default();

        let req = gc_adopt_task_request(
            "prompt".to_string(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.wait_secs, 120);
        assert_eq!(req.max_rounds, Some(3));
        assert_eq!(req.turn_timeout_secs, 600);
        assert_eq!(req.max_budget_usd, Some(0.5));
    }

    #[test]
    fn gc_adopt_task_request_respects_budget_per_signal() {
        let mut gc_config = harness_core::config::misc::GcConfig::default();
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
        let gc_config = harness_core::config::misc::GcConfig::default();
        let project_root = std::path::PathBuf::from("/tmp/my-project");

        let req = gc_adopt_task_request("prompt".to_string(), &gc_config, project_root.clone());

        assert_eq!(req.project, Some(project_root));
    }

    #[test]
    fn gc_adopt_task_request_forwards_prompt() {
        let gc_config = harness_core::config::misc::GcConfig::default();
        let prompt = "adopt draft abc123".to_string();

        let req = gc_adopt_task_request(
            prompt.clone(),
            &gc_config,
            std::path::PathBuf::from("/tmp/project"),
        );

        assert_eq!(req.prompt, Some(prompt));
    }
}

pub async fn gc_reject(
    state: &AppState,
    id: Option<serde_json::Value>,
    draft_id: DraftId,
    reason: Option<String>,
) -> RpcResponse {
    match state.engines.gc_agent.reject(&draft_id, reason.as_deref()) {
        Ok(()) => {
            log_gc_event(
                state,
                "gc_reject",
                Decision::Complete,
                Some(format!("draft={} rejected", draft_id)),
                reason,
            )
            .await;
            RpcResponse::success(id, serde_json::json!({ "rejected": true }))
        }
        Err(e) => {
            log_gc_event(
                state,
                "gc_reject",
                Decision::Warn,
                Some(format!("draft={} reject_failed: {e}", draft_id)),
                reason,
            )
            .await;
            RpcResponse::error(id, INTERNAL_ERROR, e.to_string())
        }
    }
}
