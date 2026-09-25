use super::rest_contract::ContractJson as Json;
use axum::extract::State;
use harness_protocol::rest::IntakeStatusResponse;
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;
use crate::runtime_projection::RuntimeWorkflowProjection;
use crate::task_runner;
use harness_workflow::runtime::WorkflowInstance;

#[derive(Debug)]
struct IntakeRecentDispatch {
    sort_at: Option<i64>,
    payload: serde_json::Value,
}

/// GET /api/intake — current status of all intake channels and recent dispatches.
pub(crate) async fn intake_status(
    State(state): State<Arc<AppState>>,
) -> Json<IntakeStatusResponse> {
    let intake_config = &state.core.server.config.intake;
    let all_tasks = state
        .core
        .tasks
        .as_ref()
        .map(|tasks| tasks.list_all())
        .unwrap_or_default();
    let (runtime_issue_workflows, runtime_degraded) =
        runtime_issue_workflows_for_intake_status(&state).await;
    let (issue_workflows, issue_workflows_degraded) =
        issue_workflows_for_intake_status(&state).await;

    let github_active = match issue_workflows {
        Some(workflows) => workflows
            .iter()
            .filter(|workflow| issue_workflow_is_active(workflow.state))
            .count() as u64,
        None => all_tasks
            .iter()
            .filter(|task| legacy_github_task_is_active(task))
            .count() as u64,
    } + runtime_issue_workflows
        .iter()
        .filter(|workflow| runtime_workflow_has_tracker_source(workflow, "github"))
        .filter(|workflow| !workflow.is_terminal())
        .count() as u64;

    let feishu_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            t.source.as_deref() == Some("feishu")
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let dashboard_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            (t.source.as_deref() == Some("dashboard") || t.source.is_none())
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let github_cfg = intake_config.github.as_ref();
    let github_mode =
        github_cfg.map(|config| super::github_intake_status::intake_mode_name(config.mode));
    let github_drivers = super::github_intake_status::github_intake_driver_metadata(
        github_cfg,
        &state.core.server.config.server,
        state.intake.github_pollers.len(),
    );
    let github_effective_repos = super::github_intake_status::github_effective_repos(github_cfg);
    let github_webhook_degraded = github_drivers["webhook"]["degraded"]
        .as_bool()
        .unwrap_or(false);

    let github_channel = json!({
        "name": "github",
        "enabled": github_cfg.map(|c| c.enabled).unwrap_or(false),
        "repo": github_cfg.map(|c| c.repo.as_str()).unwrap_or(""),
        "mode": github_mode,
        "drivers": github_drivers,
        "repos": github_effective_repos,
        "active": github_active,
    });

    let feishu_channel = json!({
        "name": "feishu",
        "enabled": state.intake.feishu_intake.is_some(),
        "keyword": intake_config.feishu.as_ref().map(|c| c.trigger_keyword.as_str()).unwrap_or(""),
        "active": feishu_active,
    });

    let dashboard_channel = json!({
        "name": "dashboard",
        "enabled": true,
        "active": dashboard_active,
    });

    let mut recent_dispatches: Vec<IntakeRecentDispatch> = all_tasks
        .iter()
        .filter(|t| t.source.is_some())
        .map(legacy_task_recent_dispatch)
        .collect();
    recent_dispatches.extend(
        runtime_issue_workflows
            .iter()
            .filter(|workflow| runtime_workflow_intake_source(workflow).is_some())
            .filter_map(runtime_issue_recent_dispatch),
    );
    recent_dispatches.sort_by_key(|dispatch| std::cmp::Reverse(dispatch.sort_at));
    let recent_dispatches: Vec<serde_json::Value> = recent_dispatches
        .into_iter()
        .take(10)
        .map(|dispatch| dispatch.payload)
        .collect();

    let mut response = json!({
        "channels": [github_channel, feishu_channel, dashboard_channel],
        "recent_dispatches": recent_dispatches,
    });
    if let Some(degraded) = intake_status_degraded(
        runtime_degraded,
        issue_workflows_degraded,
        github_webhook_degraded,
    ) {
        response["degraded"] = degraded;
    }
    Json(IntakeStatusResponse(response))
}

const ISSUE_WORKFLOW_INTAKE_SUBSYSTEM: &str = "issue_workflow_store";

fn intake_status_degraded(
    runtime_degraded: bool,
    issue_workflows_degraded: bool,
    github_webhook_degraded: bool,
) -> Option<serde_json::Value> {
    let mut missing = Vec::new();
    if runtime_degraded {
        missing.push("workflow_runtime_submissions");
    }
    if issue_workflows_degraded {
        missing.push(ISSUE_WORKFLOW_INTAKE_SUBSYSTEM);
    }
    if github_webhook_degraded {
        missing.push(super::github_intake_status::GITHUB_WEBHOOK_INTAKE_SUBSYSTEM);
    }
    if missing.is_empty() {
        return None;
    }
    let reason = match missing.as_slice() {
        ["workflow_runtime_submissions"] => "runtime_submission_summaries_unavailable",
        [ISSUE_WORKFLOW_INTAKE_SUBSYSTEM] => "issue_workflow_summaries_unavailable",
        [super::github_intake_status::GITHUB_WEBHOOK_INTAKE_SUBSYSTEM] => {
            "github_webhook_secret_unavailable"
        }
        _ => "intake_status_degraded",
    };
    Some(json!({
        "partial": true,
        "missing": missing,
        "reason": reason,
    }))
}

async fn issue_workflows_for_intake_status(
    state: &AppState,
) -> (
    Option<Vec<harness_workflow::issue_lifecycle::IssueWorkflowInstance>>,
    bool,
) {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return (None, false);
    };
    issue_workflow_list_outcome(store.list().await)
}

fn issue_workflow_list_outcome<E: std::fmt::Display>(
    result: Result<Vec<harness_workflow::issue_lifecycle::IssueWorkflowInstance>, E>,
) -> (
    Option<Vec<harness_workflow::issue_lifecycle::IssueWorkflowInstance>>,
    bool,
) {
    match result {
        Ok(workflows) => (Some(workflows), false),
        Err(error) => {
            tracing::error!("intake_status: issue workflow lookup failed: {error}");
            (None, true)
        }
    }
}

fn issue_workflow_is_active(state: harness_workflow::issue_lifecycle::IssueLifecycleState) -> bool {
    !matches!(
        state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
            | harness_workflow::issue_lifecycle::IssueLifecycleState::Failed
            | harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
    )
}

fn legacy_github_task_is_active(task: &task_runner::TaskState) -> bool {
    task.source.as_deref() == Some("github") && !legacy_task_status_is_terminal(&task.status)
}

fn legacy_task_status_is_terminal(status: &task_runner::TaskStatus) -> bool {
    matches!(
        status,
        task_runner::TaskStatus::Done
            | task_runner::TaskStatus::Failed
            | task_runner::TaskStatus::Cancelled
    )
}

async fn runtime_issue_workflows_for_intake_status(
    state: &AppState,
) -> (Vec<WorkflowInstance>, bool) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            Vec::new(),
            workflow_runtime_submissions_expected_but_unavailable(state),
        );
    };
    match store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
            None,
        )
        .await
    {
        Ok(workflows) => (workflows, false),
        Err(error) => {
            tracing::error!("intake_status: workflow runtime lookup failed: {error}");
            (Vec::new(), true)
        }
    }
}

fn legacy_task_recent_dispatch(task: &task_runner::TaskState) -> IntakeRecentDispatch {
    IntakeRecentDispatch {
        sort_at: parse_rfc3339_utc(task.created_at.as_deref()),
        payload: json!({
            "source": task.source,
            "external_id": task.external_id,
            "task_id": task.id.0,
            "status": serde_json::to_value(&task.status).unwrap_or(json!("unknown")),
            "pr_url": task.pr_url,
        }),
    }
}

fn runtime_issue_recent_dispatch(workflow: &WorkflowInstance) -> Option<IntakeRecentDispatch> {
    let projection = RuntimeWorkflowProjection::from_workflow_with_registry(
        &harness_workflow::runtime::WorkflowDefinitionRegistry::with_builtins(),
        workflow,
    );
    let task_id = projection.submission_handle?;
    let source = runtime_workflow_intake_source(workflow)?;
    Some(IntakeRecentDispatch {
        sort_at: Some(workflow.created_at.timestamp_micros()),
        payload: json!({
            "source": source,
            "external_id": runtime_workflow_external_id(workflow),
            "tracker_source": runtime_workflow_tracker_source(workflow),
            "tracker_external_id": runtime_workflow_tracker_external_id(workflow),
            "task_id": task_id.0,
            "status": serde_json::to_value(&projection.task_status).unwrap_or(json!("unknown")),
            "pr_url": runtime_workflow_data_string(workflow, "pr_url"),
        }),
    })
}

fn parse_rfc3339_utc(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_micros())
}

fn workflow_runtime_submissions_expected_but_unavailable(state: &AppState) -> bool {
    state
        .degraded_subsystems
        .contains(&"workflow_runtime_store")
        || state
            .startup_statuses
            .iter()
            .any(|status| status.name == "workflow_runtime_store" && !status.ready)
}

fn runtime_workflow_has_tracker_source(workflow: &WorkflowInstance, source: &str) -> bool {
    runtime_workflow_tracker_source(workflow)
        .or_else(|| runtime_workflow_data_string(workflow, "source"))
        .as_deref()
        == Some(source)
}

fn runtime_workflow_intake_source(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "source")
        .or_else(|| runtime_workflow_tracker_source(workflow))
}

fn runtime_workflow_external_id(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "external_id")
        .or_else(|| runtime_workflow_tracker_external_id(workflow))
}

fn runtime_workflow_tracker_source(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "tracker_source")
}

fn runtime_workflow_tracker_external_id(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "tracker_external_id")
}

fn runtime_workflow_data_string(workflow: &WorkflowInstance, field: &str) -> Option<String> {
    workflow
        .data
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::issue_lifecycle::IssueLifecycleState;

    #[test]
    fn issue_workflow_list_error_is_degraded_and_empty() {
        let (workflows, degraded) = issue_workflow_list_outcome::<&str>(Err("db down"));
        assert!(workflows.is_none());
        assert!(degraded);
    }

    #[test]
    fn issue_workflow_list_ok_is_not_degraded() {
        let (workflows, degraded) = issue_workflow_list_outcome::<&str>(Ok(Vec::new()));
        assert!(workflows.is_some());
        assert!(!degraded);
    }

    #[test]
    fn cancelled_issue_workflows_are_not_active() {
        assert!(!issue_workflow_is_active(IssueLifecycleState::Cancelled));
        assert!(!issue_workflow_is_active(IssueLifecycleState::Done));
        assert!(!issue_workflow_is_active(IssueLifecycleState::Failed));
        assert!(issue_workflow_is_active(IssueLifecycleState::Implementing));
    }

    #[test]
    fn cancelled_legacy_tasks_are_terminal_for_github_fallback() {
        assert!(legacy_task_status_is_terminal(
            &task_runner::TaskStatus::Cancelled
        ));
        assert!(legacy_task_status_is_terminal(
            &task_runner::TaskStatus::Done
        ));
        assert!(!legacy_task_status_is_terminal(
            &task_runner::TaskStatus::Implementing
        ));
    }

    #[test]
    fn intake_status_degraded_marks_issue_workflow_store_failure() {
        let value = intake_status_degraded(false, true, false).expect("degraded");
        assert_eq!(value["partial"], true);
        assert_eq!(value["missing"], json!([ISSUE_WORKFLOW_INTAKE_SUBSYSTEM]));
        assert_eq!(value["reason"], "issue_workflow_summaries_unavailable");
    }

    #[test]
    fn intake_status_degraded_combines_multiple_missing_subsystems() {
        let value = intake_status_degraded(true, true, false).expect("degraded");
        assert_eq!(value["reason"], "intake_status_degraded");
        assert_eq!(
            value["missing"],
            json!([
                "workflow_runtime_submissions",
                ISSUE_WORKFLOW_INTAKE_SUBSYSTEM
            ])
        );
    }

    #[test]
    fn intake_status_is_not_degraded_when_lookups_succeed() {
        assert!(intake_status_degraded(false, false, false).is_none());
    }
}
