use super::*;
use anyhow::Context;
use std::path::Path;

pub(crate) fn uses_model_scope_review(instance: &WorkflowInstance) -> bool {
    instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.definition_version == GITHUB_ISSUE_PR_DEFINITION_VERSION
        && optional_string_field(&instance.data, "definition_hash").as_deref()
            == Some(github_issue_pr_definition_hash().as_str())
}

pub(crate) fn trusted_merge_head_sha(instance: &WorkflowInstance) -> Option<String> {
    if uses_model_scope_review(instance) {
        return trusted_string_field(
            instance,
            "scope_assessed_head_oid",
            &[DataProvenance::Server],
        );
    }
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.definition_version != 1 {
        return None;
    }
    ["pr_head_sha", "head_sha"].into_iter().find_map(|field| {
        trusted_string_field(
            instance,
            field,
            &[DataProvenance::Server, DataProvenance::External],
        )
    })
}

fn trusted_string_field(
    instance: &WorkflowInstance,
    field: &str,
    allowed_provenance: &[DataProvenance],
) -> Option<String> {
    let provenance = instance
        .data_provenance
        .as_ref()?
        .provenance_for(&format!("/{field}"))?;
    if !allowed_provenance.contains(&provenance) {
        return None;
    }
    optional_string_field(&instance.data, field)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) async fn requeue_runtime_pr_scope_review_after_head_change(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    observed_head_oid: &str,
) -> anyhow::Result<bool> {
    requeue_runtime_pr_scope_review(store, instance, Some(observed_head_oid)).await
}

pub(super) async fn requeue_unassessed_runtime_pr_scope_review(
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
    if !uses_model_scope_review(&instance) || instance.state != "ready_to_merge" {
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

pub(super) fn ensure_current_classifier_policy_pin(
    instance: &WorkflowInstance,
    mut data: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    if !uses_model_scope_review(instance) {
        return Ok(data);
    }
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
        .context("current workflow is missing project_id required to pin classifier policy")?;
    crate::workflow_runtime_policy::pin_change_scope_classifier_policy(Path::new(&project_id), data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(version: u32, data: serde_json::Value) -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            version,
            "ready_to_merge",
            WorkflowSubject::new("issue", "scope-version-test"),
        )
        .with_server_data(data)
    }

    #[test]
    fn model_scope_review_requires_the_exact_current_definition_pin() {
        let current = workflow(
            GITHUB_ISSUE_PR_DEFINITION_VERSION,
            json!({ "definition_hash": github_issue_pr_definition_hash() }),
        );
        let legacy = workflow(1, json!({}));
        let malformed_current = workflow(
            GITHUB_ISSUE_PR_DEFINITION_VERSION,
            json!({ "definition_hash": "sha256:wrong" }),
        );

        assert!(uses_model_scope_review(&current));
        assert!(!uses_model_scope_review(&legacy));
        assert!(!uses_model_scope_review(&malformed_current));
    }

    #[test]
    fn trusted_merge_head_preserves_v1_and_fences_v2_to_scope_assessment() {
        let legacy = workflow(1, json!({ "pr_head_sha": "legacy-head" }));
        let current = workflow(
            GITHUB_ISSUE_PR_DEFINITION_VERSION,
            json!({
                "definition_hash": github_issue_pr_definition_hash(),
                "pr_head_sha": "unassessed-head",
                "scope_assessed_head_oid": "assessed-head",
            }),
        );

        assert_eq!(
            trusted_merge_head_sha(&legacy).as_deref(),
            Some("legacy-head")
        );
        assert_eq!(
            trusted_merge_head_sha(&current).as_deref(),
            Some("assessed-head")
        );
    }
}
