//! Active-command queries, command-status predicates, and failed-child
//! suppression for the PR feedback runtime.

use harness_workflow::runtime::{
    WorkflowCommandStatus, WorkflowRuntimeStore, LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY,
};

fn is_active_pr_feedback_command_status(status: WorkflowCommandStatus) -> bool {
    matches!(
        status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Dispatching
            | WorkflowCommandStatus::Deferred
            | WorkflowCommandStatus::Dispatched
    )
}

pub(super) async fn has_active_local_review_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            is_active_pr_feedback_command_status(record.status)
                && record.command.activity_name() == Some(LOCAL_REVIEW_ACTIVITY)
        }))
}

#[cfg(test)]
pub(super) async fn has_active_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
) -> anyhow::Result<bool> {
    has_active_pr_feedback_command_with_activity(
        store,
        workflow_id,
        failed_child_suppression_secs,
        None,
    )
    .await
}

pub(super) async fn has_active_pr_feedback_command_with_activity(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<bool> {
    let parent_has_active_command =
        store
            .commands_for(workflow_id)
            .await?
            .into_iter()
            .any(|record| {
                is_active_pr_feedback_command_status(record.status)
                    && matches!(
                        record.command.activity_name(),
                        Some("sweep_pr_feedback" | "address_pr_feedback")
                    )
                    || is_active_pr_feedback_command_status(record.status)
                        && record.command.command_type
                            == harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
                        && record
                            .command
                            .command
                            .get("definition_id")
                            .and_then(|value| value.as_str())
                            == Some(PR_FEEDBACK_DEFINITION_ID)
            });
    if parent_has_active_command {
        return Ok(true);
    }

    // Scope the child lookup to this parent's children via the indexed-by-value
    // `parent_workflow_id` column, rather than loading every PR-feedback instance
    // across all projects and filtering in memory (which scales with the whole
    // table and inflates memory use).
    for instance in store
        .list_instances_by_parent(workflow_id, None)
        .await?
        .into_iter()
        .filter(|instance| instance.definition_id == PR_FEEDBACK_DEFINITION_ID)
    {
        if matches!(instance.state.as_str(), "pending" | "inspecting")
            && has_active_child_pr_feedback_command(store, &instance.id).await?
        {
            return Ok(true);
        }
        if matches!(instance.state.as_str(), "failed" | "blocked")
            && has_recent_failed_child_pr_feedback_command(
                store,
                &instance.id,
                failed_child_suppression_secs,
                latest_pr_activity_at,
            )
            .await?
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn has_recent_failed_child_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<bool> {
    let Some(cutoff) = failed_child_suppression_cutoff(suppression_secs) else {
        return Ok(false);
    };
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            matches!(
                record.status,
                WorkflowCommandStatus::Failed | WorkflowCommandStatus::Blocked
            ) && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
                && record.updated_at >= cutoff
                && failed_child_suppression_still_applies(record.updated_at, latest_pr_activity_at)
        }))
}

fn failed_child_suppression_still_applies(
    failed_command_updated_at: chrono::DateTime<chrono::Utc>,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    latest_pr_activity_at
        .map(|activity_at| activity_at < failed_command_updated_at)
        .unwrap_or(true)
}

pub(super) fn failed_child_suppression_cutoff(
    suppression_secs: u64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if suppression_secs == 0 {
        return None;
    }
    let now = chrono::Utc::now();
    Some(
        i64::try_from(suppression_secs)
            .ok()
            .and_then(chrono::Duration::try_seconds)
            .and_then(|duration| now.checked_sub_signed(duration))
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC),
    )
}

async fn has_active_child_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            is_active_pr_feedback_command_status(record.status)
                && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
        }))
}
