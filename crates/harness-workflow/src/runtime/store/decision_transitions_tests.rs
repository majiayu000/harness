//! GH-1784 regression tests for validated decision-transition writes.

use super::*;
use crate::runtime::{WorkflowCommand, WorkflowSubject};
use chrono::{Duration, Utc};
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
    .with_server_data(json!({ "project_id": "/project-a", "pr_number": 4242 }))
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
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: Some(&initial),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-runtime-test",
        )
        .await?
        .expect("a rejected decision must remain durably observable");

    assert!(
        !record.accepted,
        "the returned record must distinguish rejection from an applied transition"
    );
    let decisions = store.decisions_for(&initial.id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(
        !decisions[0].accepted,
        "allowlist violation must remain durably recorded"
    );
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
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: Some(&initial),
                event_type: "IssueSubmitted",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-runtime-test",
        )
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

#[tokio::test]
async fn apply_decision_transition_uses_the_explicit_validation_actor() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-explicit-validation-actor", "addressing_feedback")
        .with_lease("workflow-policy", Utc::now() + Duration::minutes(5));
    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-explicit-actor-command",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);

    let record = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: Some(&initial),
                event_type: "MergeApproved",
                source: "workflow_runtime_dashboard",
                payload: json!({}),
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-policy",
        )
        .await?
        .expect("the validation actor should satisfy the active lease");

    assert!(record.accepted);
    assert_eq!(
        store
            .events_for(&initial.id)
            .await?
            .into_iter()
            .next()
            .expect("transition event")
            .source,
        "workflow_runtime_dashboard",
        "event provenance must remain independent from validator authority"
    );
    Ok(())
}

#[tokio::test]
async fn apply_decision_transition_rejects_a_mismatched_final_state() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-final-state-mismatch", "addressing_feedback");
    store
        .force_upsert_lifecycle_state_for_test(&initial)
        .await?;
    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-mismatched-state-command",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "ready_to_merge".to_string();
    final_instance.version = final_instance.version.saturating_add(1);

    let error = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: None,
                event_type: "FeedbackAddressed",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-runtime-test",
        )
        .await
        .expect_err("the persisted state must match the validated next state");

    assert!(
        error.to_string().contains("final instance state"),
        "unexpected error: {error}"
    );
    let stored = store
        .get_instance(&initial.id)
        .await?
        .expect("the original instance must remain");
    assert_eq!(stored.state, "addressing_feedback");
    assert!(store.events_for(&initial.id).await?.is_empty());
    assert!(store.decisions_for(&initial.id).await?.is_empty());
    assert!(store.commands_for(&initial.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn apply_decision_transition_treats_a_same_state_stale_snapshot_as_stale(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-same-state-stale-snapshot", "addressing_feedback");
    store
        .force_upsert_lifecycle_state_for_test(&initial)
        .await?;
    let stale_snapshot = initial.clone();
    store
        .ensure_otel_trace_context(&initial.id)
        .await?
        .expect("same-state writer should persist trace context");

    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-stale-snapshot-command",
    ));
    let mut stale_final_instance = stale_snapshot;
    stale_final_instance.state = decision.next_state.clone();
    stale_final_instance.version = stale_final_instance.version.saturating_add(1);

    let record = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: None,
                event_type: "FeedbackAddressed",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                final_instance: &stale_final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-runtime-test",
        )
        .await?;

    assert!(
        record.is_none(),
        "a stale version must not produce a record"
    );
    let stored = store
        .get_instance(&initial.id)
        .await?
        .expect("concurrent instance update must remain");
    assert_eq!(stored.state, "addressing_feedback");
    assert_eq!(stored.version, initial.version + 1);
    assert!(
        stored.data.get("otel_trace_context").is_some(),
        "the concurrent same-state update must not be overwritten"
    );
    assert!(store.events_for(&initial.id).await?.is_empty());
    assert!(store.decisions_for(&initial.id).await?.is_empty());
    assert!(store.commands_for(&initial.id).await?.is_empty());
    Ok(())
}

/// The declarative pin in `workflow.data` decides which definition — and so
/// which validator and which legal transitions — govern the instance. A
/// transition that moves it re-points the workflow at a definition it was
/// never validated against, so `definition_hash` is protected exactly like the
/// definition id and version (GH-1864).
#[tokio::test]
async fn apply_decision_transition_rejects_definition_hash_substitution() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let mut initial = instance("gh1864-definition-hash-substitution", "addressing_feedback");
    initial.set_data_field(
        "definition_hash",
        json!("a".repeat(64)),
        crate::runtime::DataProvenance::Server,
    )?;
    store
        .force_upsert_lifecycle_state_for_test(&initial)
        .await?;

    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1864-definition-hash-substitution-command",
    ));

    // Substituting the pin and removing it are both identity changes.
    let mut substituted = initial.clone();
    substituted.set_data_field(
        "definition_hash",
        json!("b".repeat(64)),
        crate::runtime::DataProvenance::Server,
    )?;
    let mut removed = initial.clone();
    removed.remove_data_field("definition_hash", crate::runtime::DataProvenance::Server)?;

    for (label, mut final_instance) in [("substituted", substituted), ("removed", removed)] {
        final_instance.state = decision.next_state.clone();
        final_instance.version = final_instance.version.saturating_add(1);

        let error = store
            .apply_decision_transition(
                WorkflowDecisionTransition {
                    expected_state: &initial.state,
                    create_if_missing: None,
                    event_type: "FeedbackAddressed",
                    source: "workflow-runtime-test",
                    payload: json!({}),
                    decision: &decision,
                    final_instance: &final_instance,
                    command_status: WorkflowCommandStatus::Pending,
                },
                "workflow-runtime-test",
            )
            .await
            .expect_err("a transition must not move the declarative definition pin");
        assert!(
            error.to_string().contains("data.definition_hash"),
            "the {label} pin must be named in the rejection: {error}"
        );

        let stored = store
            .get_instance(&initial.id)
            .await?
            .expect("the original instance must remain");
        assert_eq!(
            stored.data["definition_hash"],
            initial.data["definition_hash"]
        );
        assert_eq!(stored.state, initial.state);
        assert_eq!(stored.version, initial.version);
        assert!(store.events_for(&initial.id).await?.is_empty());
        assert!(store.decisions_for(&initial.id).await?.is_empty());
        assert!(store.commands_for(&initial.id).await?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn apply_decision_transition_rejects_definition_substitution() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = instance("gh1784-definition-substitution", "addressing_feedback");
    store
        .force_upsert_lifecycle_state_for_test(&initial)
        .await?;
    let decision = WorkflowDecision::new(
        &initial.id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "run_local_review",
        "gh1784-definition-substitution-command",
    ));
    let mut final_instance = initial.clone();
    final_instance.definition_id = "prompt_task".to_string();
    final_instance.state = decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);

    let error = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &initial.state,
                create_if_missing: None,
                event_type: "FeedbackAddressed",
                source: "workflow-runtime-test",
                payload: json!({}),
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-runtime-test",
        )
        .await
        .expect_err("a transition must not substitute the workflow definition");

    assert!(
        error.to_string().contains("definition_id"),
        "unexpected error: {error}"
    );
    let stored = store
        .get_instance(&initial.id)
        .await?
        .expect("the original instance must remain");
    assert_eq!(stored.definition_id, initial.definition_id);
    assert_eq!(stored.state, initial.state);
    assert_eq!(stored.version, initial.version);
    assert!(store.events_for(&initial.id).await?.is_empty());
    assert!(store.decisions_for(&initial.id).await?.is_empty());
    assert!(store.commands_for(&initial.id).await?.is_empty());
    Ok(())
}
