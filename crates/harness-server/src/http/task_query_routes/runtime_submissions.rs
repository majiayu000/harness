use super::*;
use crate::runtime_projection::{runtime_string_field, RuntimeWorkflowProjection};
use std::collections::HashSet;

pub(super) async fn append_runtime_submission_summaries(
    state: &AppState,
    summaries: &mut Vec<TaskSummary>,
    filter: &TaskSummaryFilter,
    cursor: Option<&TaskSummaryPageCursor>,
    limit: usize,
) -> anyhow::Result<()> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        if workflow_runtime_summaries_required(state, filter) {
            anyhow::bail!("workflow runtime store is unavailable");
        }
        return Ok(());
    };
    append_runtime_definition_summaries(store, summaries, filter, cursor, limit).await
}

fn workflow_runtime_summaries_required(state: &AppState, filter: &TaskSummaryFilter) -> bool {
    workflow_runtime_store_required(state) && filter_includes_runtime_submission_kinds(filter)
}

pub(crate) fn workflow_runtime_store_required(state: &AppState) -> bool {
    state
        .startup_statuses
        .iter()
        .any(|status| status.name == "workflow_runtime_store")
        || state
            .degraded_subsystems
            .contains(&"workflow_runtime_store")
}

fn filter_includes_runtime_submission_kinds(filter: &TaskSummaryFilter) -> bool {
    filter.kinds.is_empty()
        || filter.kinds.contains(&TaskKind::Issue)
        || filter.kinds.contains(&TaskKind::Pr)
        || filter.kinds.contains(&TaskKind::Prompt)
}

async fn append_runtime_definition_summaries(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    summaries: &mut Vec<TaskSummary>,
    filter: &TaskSummaryFilter,
    cursor: Option<&TaskSummaryPageCursor>,
    limit: usize,
) -> anyhow::Result<()> {
    let target_len = summaries.len().saturating_add(limit);
    let page_limit = i64::try_from(limit.max(50)).unwrap_or(i64::MAX);
    let mut cursor_created_at = cursor.map(|cursor| cursor.created_at);
    let mut cursor_id = cursor.map(|cursor| cursor.id.clone());
    let include_all_kinds = filter.kinds.is_empty();
    let store_filter = harness_workflow::runtime::WorkflowSubmissionFilter {
        project_id: filter.project.clone(),
        source: filter.source.clone(),
        repo: filter.repo.clone(),
        include_issue: include_all_kinds || filter.kinds.contains(&TaskKind::Issue),
        include_pr: include_all_kinds || filter.kinds.contains(&TaskKind::Pr),
        include_prompt: include_all_kinds || filter.kinds.contains(&TaskKind::Prompt),
        active_only: filter.active,
        task_statuses: filter
            .statuses
            .iter()
            .map(|status| status.as_str().to_string())
            .collect(),
    };
    let mut listed_ids: HashSet<String> = summaries
        .iter()
        .map(|summary| summary.id.as_str().to_string())
        .collect();

    loop {
        let workflows = store
            .list_submission_instances_page(
                cursor_created_at,
                cursor_id.as_deref(),
                &store_filter,
                page_limit,
            )
            .await?;
        let fetched = workflows.len();
        let next_cursor = workflows.last().and_then(|workflow| {
            RuntimeWorkflowProjection::from_workflow(workflow)
                .submission_handle
                .map(|task_id| (workflow.created_at, task_id.as_str().to_string()))
        });

        for workflow in workflows {
            let projection = RuntimeWorkflowProjection::from_workflow(&workflow);
            let Some(task_id) = projection.submission_handle.clone() else {
                continue;
            };
            let task_kind = runtime_submission_task_kind(&workflow);
            if !filter.kinds.is_empty() && !filter.kinds.contains(&task_kind) {
                continue;
            }
            if !listed_ids.insert(task_id.as_str().to_string()) {
                continue;
            }
            let summary = runtime_workflow_task_summary(workflow, task_id, task_kind);
            if filter.matches_summary(&summary) {
                summaries.push(summary);
            }
        }

        if summaries.len() >= target_len
            || fetched < usize::try_from(page_limit).unwrap_or(usize::MAX)
        {
            break;
        }
        let Some((next_created_at, next_id)) = next_cursor else {
            anyhow::bail!("runtime submission page did not expose a continuation cursor");
        };
        cursor_created_at = Some(next_created_at);
        cursor_id = Some(next_id);
    }
    Ok(())
}

pub(crate) fn runtime_submission_task_kind(
    workflow: &harness_workflow::runtime::WorkflowInstance,
) -> TaskKind {
    if workflow.definition_id == harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID {
        if workflow
            .data
            .get("issue_number")
            .and_then(|value| value.as_u64())
            .is_some()
        {
            TaskKind::Issue
        } else {
            TaskKind::Pr
        }
    } else {
        TaskKind::Prompt
    }
}

fn runtime_workflow_task_summary(
    workflow: harness_workflow::runtime::WorkflowInstance,
    task_id: harness_core::types::TaskId,
    task_kind: TaskKind,
) -> TaskSummary {
    let projection = RuntimeWorkflowProjection::from_workflow(&workflow);
    let status = projection.task_status.clone();
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let external_id = runtime_external_id(task_kind, &workflow.data, issue);
    let description = Some(runtime_submission_description(&workflow, task_kind, issue));
    TaskSummary {
        id: task_id,
        task_kind,
        status: status.clone(),
        failure_kind: projection.failure_kind,
        turn: 0,
        pr_url: runtime_string_field(&workflow.data, "pr_url"),
        error: runtime_string_field(&workflow.data, "failure_reason"),
        source: runtime_string_field(&workflow.data, "source"),
        parent_id: None,
        external_id,
        repo: runtime_string_field(&workflow.data, "repo"),
        description,
        created_at: Some(workflow.created_at.to_rfc3339()),
        phase: projection.phase,
        depends_on: runtime_task_id_array(&workflow.data, "depends_on"),
        subtask_ids: Vec::new(),
        project: projection.project_id,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        workflow: Some(TaskWorkflowSummary::from_runtime_workflow(&workflow)),
        scheduler: projection.scheduler,
    }
}

pub(crate) fn runtime_submission_description(
    workflow: &harness_workflow::runtime::WorkflowInstance,
    task_kind: TaskKind,
    issue: Option<u64>,
) -> String {
    match task_kind {
        TaskKind::Issue => issue
            .map(|issue_number| format!("issue #{issue_number}"))
            .unwrap_or_else(|| workflow.subject.subject_key.clone()),
        TaskKind::Prompt => runtime_string_field(&workflow.data, "prompt_summary")
            .unwrap_or_else(|| "prompt task".to_string()),
        _ => workflow.subject.subject_key.clone(),
    }
}

pub(crate) fn runtime_external_id(
    task_kind: TaskKind,
    workflow_data: &serde_json::Value,
    issue: Option<u64>,
) -> Option<String> {
    match task_kind {
        TaskKind::Issue => issue.map(|issue_number| format!("issue:{issue_number}")),
        TaskKind::Prompt => runtime_string_field(workflow_data, "external_id"),
        TaskKind::Pr | TaskKind::Review | TaskKind::Planner => None,
    }
}

pub(crate) fn runtime_task_id_array(data: &serde_json::Value, field: &str) -> Vec<TaskId> {
    data.get(field)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(TaskId::from_str)
        .collect()
}
