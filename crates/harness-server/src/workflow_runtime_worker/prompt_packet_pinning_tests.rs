use super::*;
use harness_workflow::runtime::{WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
use serde_json::json;

#[test]
fn prompt_contract_fails_closed_for_missing_pinned_definition_history() {
    let workflow = WorkflowInstance::new(
        "missing_declarative_history",
        42,
        "running",
        WorkflowSubject::new("test", "one"),
    )
    .with_data(json!({
        "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }));
    let contract = workflow_decision_contract(Some(&workflow));
    assert_eq!(contract["available"], false);
}

#[test]
fn forged_pin_marker_never_intercepts_builtin_prompt_contract() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", "one"),
    )
    .with_data(json!({
        "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }));
    let contract = workflow_decision_contract(Some(&workflow));
    assert_eq!(contract["available"], true);
    assert_eq!(contract["observed_state"], "discovered");
}
