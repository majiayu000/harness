use super::completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT;
use super::{ActivityResult, WorkflowCommand, WorkflowCommandType};
use serde_json::{json, Value};

pub const CHANGE_SCOPE_REVIEW_ACTIVITY: &str = "classify_change_scope";
pub const PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD: &str =
    "pinned_change_scope_classifier_policy";

pub(crate) fn has_server_classifier_assessment(result: &ActivityResult) -> bool {
    result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == ARTIFACT_CLASSIFIER_ASSESSMENT)
}

pub(crate) fn enqueue_pr_scope_review(
    dedupe_key: impl Into<String>,
    pr_number: u64,
    pr_url: &str,
    issue_plan: Value,
) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        dedupe_key,
        json!({
            "activity": CHANGE_SCOPE_REVIEW_ACTIVITY,
            "scope_facts": {
                "issue_plan": issue_plan,
                "pull_request": {
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                }
            }
        }),
    )
}

pub(crate) fn enqueue_candidate_pr_scope_review(
    event_id: &str,
    pr_number: u64,
    pr_url: &str,
) -> WorkflowCommand {
    enqueue_pr_scope_review(
        format!("candidate-promotion:{event_id}:classify-pr-scope:{pr_number}"),
        pr_number,
        pr_url,
        Value::Null,
    )
}
