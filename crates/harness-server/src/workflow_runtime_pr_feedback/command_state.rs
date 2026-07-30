use super::*;

pub(super) fn is_active_pr_feedback_command_status(status: WorkflowCommandStatus) -> bool {
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
        if handoff_child_pr_feedback_state_suppresses_duplicate_sweep(&instance.state)
            && child_state_suppression_still_applies(
                instance.updated_at,
                failed_child_suppression_secs,
                latest_pr_activity_at,
            )
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

fn handoff_child_pr_feedback_state_suppresses_duplicate_sweep(state: &str) -> bool {
    matches!(
        state,
        "feedback_found" | "no_actionable_feedback" | "ready_to_merge"
    )
}

fn child_state_suppression_still_applies(
    child_updated_at: chrono::DateTime<chrono::Utc>,
    suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    let Some(cutoff) = failed_child_suppression_cutoff(suppression_secs) else {
        return false;
    };
    child_updated_at >= cutoff
        && failed_child_suppression_still_applies(child_updated_at, latest_pr_activity_at)
}

pub(super) async fn has_active_child_pr_feedback_command(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_state_suppression_ends_after_newer_pr_activity() {
        let child_updated_at = chrono::Utc::now();
        let newer_pr_activity_at = child_updated_at + chrono::Duration::seconds(1);

        assert!(!child_state_suppression_still_applies(
            child_updated_at,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
            Some(newer_pr_activity_at),
        ));
    }

    #[test]
    fn child_state_suppression_respects_disabled_window() {
        assert!(!child_state_suppression_still_applies(
            chrono::Utc::now(),
            0,
            None,
        ));
    }

    #[test]
    fn child_state_suppression_expires_with_window() {
        let child_updated_at = chrono::Utc::now() - chrono::Duration::hours(25);

        assert!(!child_state_suppression_still_applies(
            child_updated_at,
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
            None,
        ));
    }

    #[test]
    fn child_state_suppression_applies_without_newer_activity() {
        assert!(child_state_suppression_still_applies(
            chrono::Utc::now(),
            DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
            None,
        ));
    }
}

pub(super) async fn has_recent_failed_child_pr_feedback_command(
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

pub(super) fn failed_child_suppression_still_applies(
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
