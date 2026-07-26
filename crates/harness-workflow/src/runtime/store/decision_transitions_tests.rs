//! GH-1784 regression tests: every state-changing write must be validated.

use super::*;
use crate::runtime::{WorkflowCommand, WorkflowSubject};
use harness_core::db::resolve_database_url;
use serde_json::json;

fn instance(id: &str, state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("pr", "4242"),
    )
    .with_id(id)
    .with_data(json!({ "project_id": "/project-a", "pr_number": 4242 }))
}

/// `implementing -> ready_to_merge` is absent from the github_issue_pr
/// allowlist, so the decision must be recorded as rejected and the caller's
/// `final_instance` must not be persisted.
#[tokio::test]
async fn apply_decision_transition_rejects_a_transition_outside_the_allowlist() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-apply-decision-rejects", "implementing");
    let decision = WorkflowDecision::new(
        &initial.id,
        "implementing",
        "skip_the_gates",
        "ready_to_merge",
        "jump straight to merge",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-apply-decision-command",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "ready_to_merge".to_string();
    final_instance.version = final_instance.version.saturating_add(1);

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: &initial.state,
            create_if_missing: Some(&initial),
            event_type: "IssueSubmitted",
            source: "workflow-runtime-test",
            payload: json!({}),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?
        .expect("the transition should produce a decision record");

    assert!(!record.accepted, "allowlist violation must be rejected");
    let stored = store
        .get_instance(&initial.id)
        .await?
        .expect("instance should exist");
    assert_eq!(
        stored.state, "implementing",
        "a rejected decision must not move the instance"
    );
    assert!(
        store.commands_for(&initial.id).await?.is_empty(),
        "a rejected decision must not enqueue its commands"
    );
    Ok(())
}

/// The same path must still accept an allowlisted transition.
#[tokio::test]
async fn apply_decision_transition_accepts_an_allowlisted_transition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-apply-decision-accepts", "addressing_feedback");
    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-accepts-command",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "local_review_gate".to_string();
    final_instance.version = final_instance.version.saturating_add(1);

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: &initial.state,
            create_if_missing: Some(&initial),
            event_type: "IssueSubmitted",
            source: "workflow-runtime-test",
            payload: json!({}),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?
        .expect("the transition should produce a decision record");

    assert!(record.accepted, "allowlisted transition must be accepted");
    let stored = store
        .get_instance(&initial.id)
        .await?
        .expect("instance should exist");
    assert_eq!(stored.state, "local_review_gate");
    assert_eq!(store.commands_for(&initial.id).await?.len(), 1);
    Ok(())
}
