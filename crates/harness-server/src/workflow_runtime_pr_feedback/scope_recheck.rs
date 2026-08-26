use super::*;
use anyhow::Context;
use std::path::Path;

pub(crate) async fn requeue_runtime_pr_scope_review_after_head_change(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    observed_head_oid: &str,
) -> anyhow::Result<bool> {
    requeue_runtime_pr_scope_review(store, instance, Some(observed_head_oid)).await
}

pub(super) async fn requeue_legacy_runtime_pr_scope_review(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
) -> anyhow::Result<bool> {
    requeue_runtime_pr_scope_review(store, instance, None).await
}

async fn requeue_runtime_pr_scope_review(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    observed_head_oid: Option<&str>,
) -> anyhow::Result<bool> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.state != "ready_to_merge"
    {
        return Ok(false);
    }
    let observed_head_oid = observed_head_oid
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let pr_number = instance
        .data
        .get("pr_number")
        .and_then(serde_json::Value::as_u64)
        .context("ready-to-merge workflow is missing pr_number")?;
    let pr_url = optional_string_field(&instance.data, "pr_url")
        .context("ready-to-merge workflow is missing pr_url")?;
    let issue_plan = instance
        .data
        .get("issue_plan")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let reason = if observed_head_oid.is_some() {
        "server observed a new PR head before merge approval; reassess scope before continuing"
    } else {
        "legacy merge readiness has no model-assessed head; assess current PR scope before continuing"
    };
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "reassess_pr_scope",
        "pr_scope_review",
        reason,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "ready-to-merge-scope-recheck:{}:{pr_number}:{}",
            instance.id,
            observed_head_oid.unwrap_or("legacy-unassessed")
        ),
        json!({
            "activity": harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY,
            "scope_facts": {
                "issue_plan": issue_plan,
                "pull_request": {
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                }
            }
        }),
    ));
    if let Some(observed_head_oid) = observed_head_oid {
        decision = decision.with_evidence(WorkflowEvidence::runtime_observed(
            harness_workflow::runtime::completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT,
            format!("server observed PR head {observed_head_oid}"),
            "workflow_runtime_auto_merge",
            None,
        ));
    }
    let decision = decision.high_confidence();
    let accepted_data = instance.data.clone();
    let workflow_id = instance.id.clone();
    let outcome = commit_runtime_decision(
        store,
        instance,
        false,
        decision,
        "PrHeadChanged",
        "workflow_runtime_auto_merge",
        json!({
            "workflow_id": workflow_id,
            "pr_number": pr_number,
            "pr_url": pr_url,
            "observed_head_oid": observed_head_oid,
            "migration": observed_head_oid.is_none(),
        }),
        accepted_data,
    )
    .await?;
    match outcome {
        RuntimeDecisionCommitOutcome::Accepted => Ok(true),
        RuntimeDecisionCommitOutcome::Stale => Ok(false),
        RuntimeDecisionCommitOutcome::Rejected { reason } => {
            anyhow::bail!("scope reassessment transition was rejected: {reason}")
        }
    }
}

pub(super) fn pin_legacy_classifier_policy(
    instance: &WorkflowInstance,
    mut data: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    if data
        .get(crate::workflow_runtime_policy::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD)
        .is_some()
    {
        return Ok(data);
    }
    if let Some(existing_pin) = instance
        .data
        .get(crate::workflow_runtime_policy::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD)
        .cloned()
    {
        data.as_object_mut()
            .context("workflow policy pin requires object workflow data")?
            .insert(
                crate::workflow_runtime_policy::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD
                    .to_string(),
                existing_pin,
            );
        return Ok(data);
    }
    let project_id = data
        .get("project_id")
        .or_else(|| instance.data.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .context("legacy workflow is missing project_id required to pin classifier policy")?;
    crate::workflow_runtime_policy::pin_change_scope_classifier_policy(Path::new(&project_id), data)
}
