use crate::github_pr_snapshot::{GitHubPrSnapshotArtifacts, GitHubPrSnapshotTarget};
use crate::http::AppState;
use harness_core::config::workflow::WorkflowConfig;
use harness_workflow::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeJobMutationFence, RuntimeJobStatus,
    WorkflowCommandRecord, WorkflowCommandStatus, WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub(in crate::workflow_runtime_worker) async fn current_merge_authorization(
    state: &AppState,
    job: &RuntimeJob,
) -> Result<WorkflowInstance, String> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| "workflow runtime store is unavailable".to_string())?;
    let workflow_id = job
        .input
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "merge job is missing its workflow identity".to_string())?;
    let workflow = store
        .get_instance(workflow_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("workflow `{workflow_id}` no longer exists"))?;
    let command = store
        .get_command(&job.command_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("merge command `{}` no longer exists", job.command_id))?;
    let persisted_job = store
        .get_runtime_job(&job.id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("merge job `{}` no longer exists", job.id))?;
    validate_merge_authorization(job, &workflow, &command, &persisted_job)?;
    Ok(workflow)
}

pub(in crate::workflow_runtime_worker) async fn fenced_merge_authorization(
    state: &AppState,
    job: &RuntimeJob,
) -> Result<RuntimeJobMutationFence, String> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| "workflow runtime store is unavailable".to_string())?;
    let fence = store
        .fence_runtime_job_mutation(&job.id, &job.command_id)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = validate_fenced_merge_authorization(job, &fence) {
        fence
            .release()
            .await
            .map_err(|release_error| format!("{error}; fence release failed: {release_error}"))?;
        return Err(error);
    }
    Ok(fence)
}

pub(in crate::workflow_runtime_worker) fn validate_fenced_merge_authorization(
    expected_job: &RuntimeJob,
    fence: &RuntimeJobMutationFence,
) -> Result<(), String> {
    validate_merge_authorization(expected_job, &fence.workflow, &fence.command, &fence.job)
}

fn validate_merge_authorization(
    expected_job: &RuntimeJob,
    workflow: &WorkflowInstance,
    command: &WorkflowCommandRecord,
    persisted_job: &RuntimeJob,
) -> Result<(), String> {
    let workflow_id = expected_job
        .input
        .get("workflow_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "merge job is missing its workflow identity".to_string())?;
    if workflow.id != workflow_id
        || workflow.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        || workflow.state != "merging"
    {
        return Err(format!(
            "workflow `{workflow_id}` is no longer in the authorized merging state"
        ));
    }
    if command.workflow_id != workflow.id
        || command.id != expected_job.command_id
        || command.status != WorkflowCommandStatus::Dispatched
        || command.superseded_by_command_id.is_some()
        || command.command.activity_name() != Some("merge_pr")
        || expected_job.input.get("command") != Some(&command.command.command)
    {
        return Err(format!(
            "merge command `{}` is stale, superseded, or does not match its immutable job envelope",
            expected_job.command_id
        ));
    }
    if persisted_job.id != expected_job.id
        || persisted_job.status != RuntimeJobStatus::Running
        || persisted_job.command_id != expected_job.command_id
        || persisted_job.lease_generation != expected_job.lease_generation
        || persisted_job.input.get("workflow_id") != expected_job.input.get("workflow_id")
        || persisted_job.input.get("command") != expected_job.input.get("command")
        || persisted_job.input.get("cancellation_requested").is_some()
        || persisted_job
            .lease
            .as_ref()
            .map(|lease| lease.owner.as_str())
            != expected_job
                .lease
                .as_ref()
                .map(|lease| lease.owner.as_str())
        || persisted_job
            .lease
            .as_ref()
            .is_none_or(|lease| lease.expires_at <= chrono::Utc::now())
    {
        return Err(format!(
            "merge job `{}` no longer owns its current execution lease",
            expected_job.id
        ));
    }
    Ok(())
}

pub(in crate::workflow_runtime_worker) async fn finish_server_merge(
    _state: &AppState,
    _job: &RuntimeJob,
    activity: String,
    target: &GitHubPrSnapshotTarget,
    snapshot: GitHubPrSnapshotArtifacts,
    expected_head_sha: &str,
    outcome: &str,
    mut merge_call: Option<Value>,
    delete_branch: bool,
    _github_token: Option<&str>,
) -> ActivityResult {
    if delete_branch {
        let cleanup = safe_branch_cleanup_outcome(&snapshot.normalized_snapshot);
        merge_call.get_or_insert_with(|| json!({}))["branch_cleanup"] = cleanup;
    }
    super::super::server_merge::server_merge_succeeded(
        activity,
        target,
        snapshot,
        expected_head_sha,
        outcome,
        merge_call,
    )
}

fn safe_branch_cleanup_outcome(snapshot: &Value) -> Value {
    match snapshot.get("is_cross_repository").and_then(Value::as_bool) {
        Some(true) => json!({"status": "skipped_cross_repository"}),
        Some(false) => json!({
            "status": "skipped_atomic_delete_unavailable",
            "reason": "GitHub does not provide an expected-SHA compare-and-delete operation for refs"
        }),
        None => json!({"status": "skipped_repository_ownership_unknown"}),
    }
}

pub(super) async fn prepare_classifier(
    state: &Arc<AppState>,
    job: &mut RuntimeJob,
    workflow: &mut Option<WorkflowInstance>,
    config: &WorkflowConfig,
) -> anyhow::Result<()> {
    normalize_classifier_input(job)?;
    let Some(current) = workflow.as_ref() else {
        return Ok(());
    };
    if current
        .data
        .get(harness_workflow::runtime::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD)
        .is_some()
    {
        return Ok(());
    }
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workflow runtime store is unavailable"))?;
    let activity = super::super::data_helpers::activity_name(job);
    let Some(definition) = store
        .definition_registry()
        .declarative_definition_for_instance(current)
    else {
        return Ok(());
    };
    if !definition.requires_server_classifier_assessment(&current.state)
        || definition.classifier_activity_policy(&activity).is_some()
    {
        return Ok(());
    }
    let policy = config.activities.get(&activity).ok_or_else(|| {
        anyhow::anyhow!(
            "legacy classifier activity `{activity}` has no policy in the active workflow config"
        )
    })?;
    if policy.classifier.is_none() {
        anyhow::bail!(
            "legacy classifier activity `{activity}` is missing its classifier contract in the active workflow config"
        );
    }
    *workflow = Some(
        store
            .backfill_change_scope_classifier_policy_if_missing(
                &current.id,
                serde_json::to_value(policy)?,
                "runtime_worker_classifier_migration",
            )
            .await?,
    );
    Ok(())
}

pub(in crate::workflow_runtime_worker) fn normalize_classifier_input(
    job: &mut RuntimeJob,
) -> anyhow::Result<()> {
    let Some(scope_facts) = job.input.pointer("/command/scope_facts").cloned() else {
        return Ok(());
    };
    job.input
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("classifier runtime job input must be an object"))?
        .insert("scope_facts".to_string(), scope_facts);
    Ok(())
}

pub(super) async fn execute(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<ActivityResult>> {
    if job
        .input
        .get("server_owned_remote_rejection")
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        let activity = super::super::data_helpers::activity_name(job);
        return Ok(Some(
            ActivityResult::failed(
                activity,
                "Legacy remote classifier was rejected before execution.",
                "classifier activities must use a local agent runtime; update runtime_dispatch and retry",
            )
            .with_error_kind(ActivityErrorKind::Configuration),
        ));
    }
    match super::super::data_helpers::activity_name(job).as_str() {
        "start_child_workflow" => Ok(Some(
            super::super::child_workflow::execute_start_child_workflow(state, job, parent).await?,
        )),
        activity if activity == harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY => {
            Ok(Some(
                super::super::pr_feedback_inspection::execute_pr_feedback_inspection(
                    state, job, parent,
                )
                .await,
            ))
        }
        "merge_pr"
            if parent.is_some_and(|workflow| {
                harness_workflow::runtime::is_github_merge_activity(workflow, "merge_pr")
            }) =>
        {
            if super::super::server_merge::server_merge_execution_enabled(state, job, parent) {
                Ok(Some(
                    super::super::server_merge::execute_server_merge(state, job, parent).await,
                ))
            } else {
                Ok(Some(
                    ActivityResult::failed(
                        "merge_pr",
                        "Unsafe agent merge execution was rejected before mutation.",
                        "automated merges require merge_execution=server and the server mutation fence",
                    )
                    .with_error_kind(ActivityErrorKind::Configuration),
                ))
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_repository_branch_cleanup_declines_non_atomic_deletion() {
        let outcome = safe_branch_cleanup_outcome(&json!({"is_cross_repository": false}));

        assert_eq!(outcome["status"], "skipped_atomic_delete_unavailable");
    }
}
