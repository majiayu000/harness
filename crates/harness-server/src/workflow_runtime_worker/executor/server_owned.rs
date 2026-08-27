use crate::http::AppState;
use harness_core::config::workflow::WorkflowConfig;
use harness_workflow::runtime::{ActivityErrorKind, ActivityResult, RuntimeJob, WorkflowInstance};
use std::sync::Arc;

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
            if super::super::server_merge::server_merge_execution_enabled(state, job, parent) =>
        {
            Ok(Some(
                super::super::server_merge::execute_server_merge(state, job, parent).await,
            ))
        }
        _ => Ok(None),
    }
}
