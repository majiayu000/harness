use harness_core::config::intake::ResolvedGitHubAutoMergePolicy;
use harness_workflow::runtime::{DataProvenance, WorkflowDataWrite, WorkflowInstance};
use serde_json::Value;

use crate::github_pr_snapshot::{value_string, GitHubPrSnapshotArtifacts};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AutoMergeSnapshotGate {
    Ready(Box<WorkflowInstance>),
    NotReady,
}

pub(crate) fn prepare_auto_merge_workflow_from_snapshot(
    workflow: &WorkflowInstance,
    snapshot: &GitHubPrSnapshotArtifacts,
    policy: &ResolvedGitHubAutoMergePolicy,
) -> anyhow::Result<AutoMergeSnapshotGate> {
    let expected_base_ref = expected_base_ref_from_workflow_data(&workflow.data);
    if !auto_merge_snapshot_satisfies_policy(
        &snapshot.normalized_snapshot,
        policy,
        expected_base_ref.as_deref(),
    ) {
        return Ok(AutoMergeSnapshotGate::NotReady);
    }
    let Some(snapshot_head_sha) = value_string(snapshot.normalized_snapshot.get("head_oid")) else {
        return Ok(AutoMergeSnapshotGate::NotReady);
    };
    let expected_head_sha = snapshot_head_sha.as_str();

    let remote_fact = snapshot.remote_fact_snapshot()?;
    let mut workflow = workflow.clone();
    if !workflow.data.is_object() {
        workflow.replace_classified_data(serde_json::json!({}), DataProvenance::Server);
    }
    let mut writes = vec![
        WorkflowDataWrite::set(
            "merge_policy",
            serde_json::json!("auto"),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "merge_method",
            serde_json::json!(policy.method.to_string()),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "merge_delete_branch",
            serde_json::json!(policy.delete_branch),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "merge_require_review_threads_resolved",
            serde_json::json!(policy.require_review_threads_resolved),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "merge_require_clean_merge_state",
            serde_json::json!(policy.require_clean_merge_state),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "merge_execution",
            serde_json::json!(policy.merge_execution.to_string()),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "verify_merge_completion",
            serde_json::json!(policy.verify_merge_completion),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "last_remote_fact_hash",
            serde_json::json!(remote_fact.fact_hash),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "pr_head_sha",
            serde_json::json!(snapshot_head_sha),
            DataProvenance::External,
        ),
        WorkflowDataWrite::set(
            "merge_attempted_head_sha",
            serde_json::json!(expected_head_sha),
            DataProvenance::External,
        ),
    ];
    if let Some(expected_base_ref) = expected_base_ref {
        writes.push(WorkflowDataWrite::set(
            "expected_base_ref",
            serde_json::json!(expected_base_ref),
            DataProvenance::External,
        ));
    }
    if let Some(pr_url) = value_string(snapshot.normalized_snapshot.get("pr_url")) {
        writes.push(WorkflowDataWrite::set(
            "pr_url",
            serde_json::json!(pr_url),
            DataProvenance::External,
        ));
    }
    workflow.apply_data_writes(writes)?;
    Ok(AutoMergeSnapshotGate::Ready(Box::new(workflow)))
}

pub(crate) fn auto_merge_snapshot_satisfies_policy(
    snapshot: &Value,
    policy: &ResolvedGitHubAutoMergePolicy,
    expected_base_ref: Option<&str>,
) -> bool {
    if snapshot_string_eq(snapshot, "state", "MERGED")
        || snapshot_string_eq(snapshot, "state", "CLOSED")
    {
        return false;
    }
    if !snapshot_string_eq(snapshot, "status_check_rollup_state", "SUCCESS")
        || !snapshot_string_eq(snapshot, "review_decision", "APPROVED")
        || snapshot.get("is_draft").and_then(Value::as_bool) != Some(false)
    {
        return false;
    }
    if policy.require_clean_merge_state
        && !snapshot_string_eq(snapshot, "merge_state_status", "CLEAN")
    {
        return false;
    }
    if !snapshot_base_ref_matches_expected(snapshot, expected_base_ref) {
        return false;
    }
    if policy.require_review_threads_resolved {
        let review_threads_complete = snapshot
            .get("review_threads_complete")
            .and_then(Value::as_bool);
        let unresolved_count = snapshot
            .get("active_unresolved_review_threads_count")
            .and_then(Value::as_u64);
        if review_threads_complete != Some(true) || unresolved_count != Some(0) {
            return false;
        }
    }
    true
}

pub(crate) fn expected_base_ref_from_workflow_data(data: &Value) -> Option<String> {
    ["expected_base_ref", "target_base_ref", "base_ref"]
        .into_iter()
        .find_map(|field| value_string(data.get(field)))
}

fn snapshot_base_ref_matches_expected(snapshot: &Value, expected_base_ref: Option<&str>) -> bool {
    let Some(expected_base_ref) = expected_base_ref else {
        return true;
    };
    let expected_base_ref = expected_base_ref.trim();
    !expected_base_ref.is_empty()
        && value_string(snapshot.get("base_ref"))
            .is_some_and(|base_ref| base_ref == expected_base_ref)
}

fn snapshot_string_eq(snapshot: &Value, field: &str, expected: &str) -> bool {
    snapshot
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}
