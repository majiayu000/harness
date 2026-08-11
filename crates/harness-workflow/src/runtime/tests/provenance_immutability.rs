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

fn cancellation_marker(dedupe_key: &str) -> WorkflowCommand {
    WorkflowCommand::new(WorkflowCommandType::MarkCancelled, dedupe_key, json!({}))
}

fn cancellation_decision(workflow_id: &str, marker: &WorkflowCommand) -> WorkflowDecision {
    WorkflowDecision::new(
        workflow_id,
        "running",
        "cancel_submission",
        "cancelled",
        "operator cancelled the submission",
    )
    .with_command(marker.clone())
}

async fn force_cancelled(
    store: &WorkflowRuntimeStore,
    instance: &mut WorkflowInstance,
) -> anyhow::Result<()> {
    instance.state = "cancelled".to_string();
    store.force_upsert_lifecycle_state_for_test(instance).await
}

/// Cleanup may proceed: the live marker was minted by the latest accepted
/// decision, and that decision is what placed the instance in `cancelled`.
#[tokio::test]
async fn cancellation_cleanup_honors_marker_bound_to_current_decision() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = workflow("gh1865-cancel-bound", "running");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let marker = cancellation_marker("gh1865-cancel-bound-marker");
    let record =
        WorkflowDecisionRecord::accepted(cancellation_decision(&instance.id, &marker), None);
    store.record_decision(&record).await?;
    store
        .enqueue_command(&instance.id, Some(&record.id), &marker)
        .await?;
    force_cancelled(&store, &mut instance).await?;

    let stored = store
        .get_instance(&instance.id)
        .await?
        .expect("instance must exist");
    let outcome = store
        .finish_cancellation_cleanup_if_current(&stored, "test", "cleanup")
        .await?;
    let WorkflowCancellationCleanupOutcome::Cleaned(cleaned) = outcome else {
        panic!("a marker bound to the current decision must authorize cleanup, got: {outcome:?}");
    };
    assert_eq!(cleaned.data["cancelled"], true);
    Ok(())
}

/// A marker row with no decision behind it proves nothing about what was
/// authorized, so it must not authorize cancelling the generation's work.
#[tokio::test]
async fn cancellation_cleanup_rejects_detached_marker() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = workflow("gh1865-cancel-detached", "running");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let activity = WorkflowCommand::enqueue_activity("implement", "gh1865-detached-activity");
    let activity_id = store.enqueue_command(&instance.id, None, &activity).await?;
    let marker = cancellation_marker("gh1865-cancel-detached-marker");
    store.enqueue_command(&instance.id, None, &marker).await?;
    force_cancelled(&store, &mut instance).await?;

    let stored = store
        .get_instance(&instance.id)
        .await?
        .expect("instance must exist");
    let outcome = store
        .finish_cancellation_cleanup_if_current(&stored, "test", "cleanup")
        .await?;
    assert_eq!(
        outcome,
        WorkflowCancellationCleanupOutcome::NoCancellationCommand
    );

    let activity = store
        .get_command(&activity_id)
        .await?
        .expect("activity command must remain queryable");
    assert_eq!(
        activity.status,
        WorkflowCommandStatus::Pending,
        "a detached marker must not cancel the generation's live commands"
    );
    Ok(())
}

/// A rejection never authorized anything; a marker pointing at a rejected
/// decision must not authorize cleanup either.
#[tokio::test]
async fn cancellation_cleanup_rejects_marker_for_rejected_decision() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = workflow("gh1865-cancel-rejected", "running");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let marker = cancellation_marker("gh1865-cancel-rejected-marker");
    let record = WorkflowDecisionRecord::rejected(
        cancellation_decision(&instance.id, &marker),
        None,
        "transition is outside the allowlist",
    );
    store.record_decision(&record).await?;
    store
        .enqueue_command(&instance.id, Some(&record.id), &marker)
        .await?;
    force_cancelled(&store, &mut instance).await?;

    let stored = store
        .get_instance(&instance.id)
        .await?
        .expect("instance must exist");
    let outcome = store
        .finish_cancellation_cleanup_if_current(&stored, "test", "cleanup")
        .await?;
    assert_eq!(
        outcome,
        WorkflowCancellationCleanupOutcome::NoCancellationCommand
    );
    Ok(())
}

/// The marker belongs to the generation that was cancelled. Once a newer
/// accepted decision moved the workflow on, the old marker is history and
/// must not authorize cleaning up the new generation.
#[tokio::test]
async fn cancellation_cleanup_rejects_marker_from_older_generation() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = workflow("gh1865-cancel-old-generation", "running");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let marker = cancellation_marker("gh1865-cancel-old-generation-marker");
    let cancelled_record =
        WorkflowDecisionRecord::accepted(cancellation_decision(&instance.id, &marker), None);
    store.record_decision(&cancelled_record).await?;
    store
        .enqueue_command(&instance.id, Some(&cancelled_record.id), &marker)
        .await?;
    force_cancelled(&store, &mut instance).await?;

    // A newer accepted decision reopened the workflow; the instance row in
    // this fixture still says `cancelled`, which is exactly the stale view a
    // racing cleanup would hold.
    let mut reopened = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            &instance.id,
            "cancelled",
            "reopen_submission",
            "planning",
            "operator reopened the submission",
        ),
        None,
    );
    reopened.created_at = cancelled_record.created_at + chrono::Duration::seconds(1);
    store.record_decision(&reopened).await?;

    let stored = store
        .get_instance(&instance.id)
        .await?
        .expect("instance must exist");
    let outcome = store
        .finish_cancellation_cleanup_if_current(&stored, "test", "cleanup")
        .await?;
    assert_eq!(
        outcome,
        WorkflowCancellationCleanupOutcome::NoCancellationCommand
    );
    Ok(())
}

/// Superseding retires an attempt (GH-1865 W2). A superseded marker is part
/// of the historical record and must not authorize cleanup, even when it was
/// bound to an accepted cancellation decision when it was live.
#[tokio::test]
async fn cancellation_cleanup_rejects_superseded_marker() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = workflow("gh1865-cancel-superseded", "running");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let marker = cancellation_marker("gh1865-cancel-superseded-marker");
    let cancelled_record =
        WorkflowDecisionRecord::accepted(cancellation_decision(&instance.id, &marker), None);
    store.record_decision(&cancelled_record).await?;
    let marker_id = store
        .enqueue_command(&instance.id, Some(&cancelled_record.id), &marker)
        .await?;

    // A newer decision reuses the marker's dedupe key for different work,
    // superseding the marker attempt.
    let replacement =
        WorkflowCommand::enqueue_activity("implement", "gh1865-cancel-superseded-marker");
    let mut replacement_record = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            &instance.id,
            "cancelled",
            "schedule_rework",
            "cancelled",
            "rework scheduled over the cancelled attempt",
        )
        .with_command(replacement.clone()),
        None,
    );
    replacement_record.created_at = cancelled_record.created_at + chrono::Duration::seconds(1);
    store.record_decision(&replacement_record).await?;
    store
        .enqueue_command(&instance.id, Some(&replacement_record.id), &replacement)
        .await?;
    force_cancelled(&store, &mut instance).await?;

    let marker = store
        .get_command(&marker_id)
        .await?
        .expect("marker command must remain queryable");
    assert_eq!(marker.status, WorkflowCommandStatus::Superseded);

    let stored = store
        .get_instance(&instance.id)
        .await?
        .expect("instance must exist");
    let outcome = store
        .finish_cancellation_cleanup_if_current(&stored, "test", "cleanup")
        .await?;
    assert_eq!(
        outcome,
        WorkflowCancellationCleanupOutcome::NoCancellationCommand
    );
    Ok(())
}
