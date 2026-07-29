use axum::{extract::State, Json};
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
pub(crate) async fn intake_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let intake_config = &state.core.server.config.intake;
    let all_tasks = state.core.tasks.list_all();
    let (runtime_issue_workflows, runtime_degraded) =
        runtime_issue_workflows_for_intake_status(&state).await;

    let github_active: u64 = if let Some(store) = state.core.issue_workflow_store.as_ref() {
        match store.list().await {
            Ok(workflows) => workflows
                .into_iter()
                .filter(|workflow| {
                    !matches!(
                        workflow.state,
                        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Failed
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
                    )
                })
                .count() as u64,
            Err(_) => all_tasks
                .iter()
                .filter(|t| {
                    t.source.as_deref() == Some("github")
                        && !matches!(
                            t.status,
                            task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                        )
                })
                .count() as u64,
        }
    } else {
        all_tasks
            .iter()
            .filter(|t| {
                t.source.as_deref() == Some("github")
                    && !matches!(
                        t.status,
                        task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                    )
            })
            .count() as u64
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
    let mut degraded_missing = Vec::new();
    if runtime_degraded {
        degraded_missing.push("workflow_runtime_submissions");
    }
    if github_webhook_degraded {
        degraded_missing.push(super::github_intake_status::GITHUB_WEBHOOK_INTAKE_SUBSYSTEM);
    }
    if !degraded_missing.is_empty() {
        let reason = if runtime_degraded && !github_webhook_degraded {
            "runtime_submission_summaries_unavailable"
        } else if github_webhook_degraded && !runtime_degraded {
            "github_webhook_secret_unavailable"
        } else {
            "intake_status_degraded"
        };
        response["degraded"] = json!({
            "partial": true,
            "missing": degraded_missing,
            "reason": reason,
        });
    }
    Json(response)
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
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
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
