//! GH-1865 regression tests: workflow audit records are append-only.

use super::*;
use crate::runtime::{DecisionProvenanceConflict, PromptPayloadIntegrityError};

fn workflow(id: &str, state: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("pr", "9101"),
    )
    .with_id(id)
    .with_server_data(json!({ "project_id": "/project-a", "pr_number": 9101 }))
}

fn decision(workflow_id: &str) -> WorkflowDecision {
    WorkflowDecision::new(
        workflow_id,
        "addressing_feedback",
        "address_feedback",
        "local_review_gate",
        "feedback addressed",
    )
}

/// Replaying the exact same decision row is how retries and submission replays
/// behave, so it must stay a no-op rather than an error.
#[tokio::test]
async fn decision_replay_with_identical_content_is_idempotent() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = workflow("gh1865-idempotent-replay", "addressing_feedback");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let record = WorkflowDecisionRecord::accepted(decision(&instance.id), None);
    store.record_decision(&record).await?;
    store.record_decision(&record).await?;

    let stored = store.decisions_for(&instance.id).await?;
    assert_eq!(stored.len(), 1, "a replay must not duplicate the audit row");
    assert_eq!(stored[0], record);
    Ok(())
}

/// The audit trail records what was authorized. A later write reusing a
/// decision id must not be able to rewrite that: flipping `accepted` would
/// turn a rejection into an authorization after the fact, and rewriting the
/// decision content would make one id describe a transition it never
/// authorized.
#[tokio::test]
async fn decision_rewrite_under_an_existing_id_is_refused() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = workflow("gh1865-rewrite-refused", "addressing_feedback");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let original = WorkflowDecisionRecord::rejected(
        decision(&instance.id),
        None,
        "transition is outside the allowlist",
    );
    store.record_decision(&original).await?;

    let mut promoted = original.clone();
    promoted.accepted = true;
    promoted.rejection_reason = None;

    let mut rewritten_content = original.clone();
    rewritten_content.decision.next_state = "merging".to_string();

    let mut relinked = original.clone();
    relinked.event_id = Some("some-other-event".to_string());

    for (label, attempt, expected_field) in [
        ("a rejection promoted to accepted", promoted, "accepted"),
        (
            "a rewritten decision body",
            rewritten_content,
            "rejection_reason",
        ),
        ("a re-linked event", relinked, "event_id"),
    ] {
        let error = store
            .record_decision(&attempt)
            .await
            .expect_err("rewriting an existing decision id must fail closed");
        let conflict = error
            .downcast_ref::<DecisionProvenanceConflict>()
            .unwrap_or_else(|| panic!("{label} must be a provenance conflict, got: {error}"));
        assert_eq!(conflict.decision_id, original.id);
        assert!(
            conflict.changed_fields.contains(&expected_field)
                || conflict.changed_fields.contains(&"data"),
            "{label} must name what moved, got: {:?}",
            conflict.changed_fields
        );

        let stored = store.decisions_for(&instance.id).await?;
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0], original,
            "{label} must leave the original record untouched"
        );
    }
    Ok(())
}

/// A `prompt_ref` is content-addressed: every record that stores one resolves
/// through it to the prompt that was actually run. Rebinding it to different
/// bytes would silently rewrite what those records mean, so equal bytes are an
/// idempotent replay and different bytes are refused.
#[tokio::test]
async fn prompt_payload_refs_are_insert_once() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let prompt_ref = "prompt-memory:gh1865-insert-once";
    store
        .insert_prompt_payload(prompt_ref, "implement the fix")
        .await?;
    store
        .insert_prompt_payload(prompt_ref, "implement the fix")
        .await?;
    assert_eq!(
        store.get_prompt_payload(prompt_ref).await?,
        Some("implement the fix".to_string()),
        "an identical replay must leave the payload untouched"
    );

    let error = store
        .insert_prompt_payload(prompt_ref, "do something else entirely")
        .await
        .expect_err("rebinding a prompt ref to different bytes must fail closed");
    let integrity = error
        .downcast_ref::<PromptPayloadIntegrityError>()
        .unwrap_or_else(|| panic!("expected an integrity error, got: {error}"));
    assert_eq!(integrity.prompt_ref, prompt_ref);
    assert_eq!(
        store.get_prompt_payload(prompt_ref).await?,
        Some("implement the fix".to_string()),
        "the stored prompt must survive a rejected rebind"
    );
    Ok(())
}
