use super::*;
use crate::runtime::model::{WorkflowCommandType, WorkflowEvidence, WorkflowSubject};
use crate::runtime::{DataProvenance, PromptContinuationPolicy};
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::collections::BTreeSet;

fn pin_error_instance(id: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "missing_declarative_definition",
        42,
        "running",
        WorkflowSubject::new("test", id),
    )
    .with_id(id)
    .with_server_data(json!({
        "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    }))
}

fn pin_safety_decision(instance: &WorkflowInstance) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "definition_version_missing",
        "blocked",
        "pinned definition is unavailable",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkBlocked,
        "pin:block",
        json!({ "reason": "pinned definition is unavailable" }),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        "pin:operator",
        json!({ "reason": "pinned definition is unavailable" }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "definition_pin_error",
        "definition=missing_declarative_definition version=42 error=missing_version",
    ))
}

#[test]
fn parent_pr_inspection_persists_server_observed_head_for_manual_merge() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "start_quality_gate",
        "quality_gate_pending",
        "PR inspection found the PR ready for validation.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "snapshot_source": "server_github_graphql",
                        "repo": "owner/repo",
                        "pr_number": 77,
                        "pr_url": "https://github.com/owner/repo/pull/77",
                        "head_oid": "server-head-77",
                        "observed_at": "2026-08-15T00:00:00Z"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["pr_head_sha"], "server-head-77");
    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|sidecar| sidecar.provenance_for("/pr_head_sha")),
        Some(DataProvenance::External)
    );
    Ok(())
}

#[test]
fn parent_pr_inspection_tracks_and_resets_feedback_repair_progress() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let repair_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "address_pr_feedback",
        "addressing_feedback",
        "PR inspection found actionable feedback.",
    );
    let repair_event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": "sweep_pr_feedback",
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "snapshot_source": "server_github_graphql",
                        "repo": "owner/repo",
                        "pr_number": 77,
                        "pr_url": "https://github.com/owner/repo/pull/77",
                        "actionable_blocker_count": 2,
                        "observed_at": "2026-08-15T00:00:00Z"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &repair_decision, &repair_event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 2);

    let pending_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "PR inspection is waiting for hosted checks.",
    );
    let pending_event = WorkflowEvent::new(&instance.id, 2, "RuntimeJobCompleted", "runtime-2")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "snapshot_source": "server_github_graphql",
                        "repo": "owner/repo",
                        "pr_number": 77,
                        "pr_url": "https://github.com/owner/repo/pull/77",
                        "actionable_blocker_count": 0,
                        "status_check_rollup_state": "PENDING",
                        "observed_at": "2026-08-15T00:05:00Z"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &pending_decision, &pending_event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 2);

    let converged_event = WorkflowEvent::new(&instance.id, 3, "RuntimeJobCompleted", "runtime-3")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "snapshot_source": "server_github_graphql",
                        "repo": "owner/repo",
                        "pr_number": 77,
                        "pr_url": "https://github.com/owner/repo/pull/77",
                        "actionable_blocker_count": 0,
                        "status_check_rollup_state": "SUCCESS",
                        "observed_at": "2026-08-15T00:10:00Z"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &pending_decision, &converged_event)?;

    assert!(instance.data.get("feedback_repair_round").is_none());
    assert!(instance.data.get("feedback_repair_blocker_count").is_none());
    Ok(())
}

#[test]
fn agent_pr_feedback_snapshot_cannot_clear_feedback_repair_history() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "feedback_repair_round": 1,
        "feedback_repair_blocker_count": 2,
    }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "Agent claimed feedback convergence.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "artifacts": [{
                    "artifact_type": crate::runtime::PR_FEEDBACK_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "actionable_blocker_count": 0,
                        "status_check_rollup_state": "SUCCESS",
                        "observed_at": "2026-08-31T00:00:00Z"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 2);
    Ok(())
}

#[test]
fn mismatched_server_snapshot_cannot_clear_feedback_repair_history() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "feedback_repair_round": 1,
        "feedback_repair_blocker_count": 2,
    }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "A stale snapshot reported convergence.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": {
                        "snapshot_source": "server_github_graphql",
                        "repo": "owner/other",
                        "pr_number": 77,
                        "pr_url": "https://github.com/owner/other/pull/77",
                        "actionable_blocker_count": 0,
                        "status_check_rollup_state": "SUCCESS"
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 2);
    Ok(())
}

#[test]
fn local_review_repair_advances_feedback_repair_progress() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "local_review_gate",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "pr_number": 77,
        "feedback_repair_round": 1,
        "feedback_repair_blocker_count": 2,
    }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "local_review_gate",
        "address_local_review_feedback",
        "addressing_feedback",
        "Local review found one remaining blocker.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::LOCAL_REVIEW_ACTIVITY,
                "signals": [{
                    "signal_type": crate::runtime::LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
                    "signal": {
                        "actionable_blocker_count": 1
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["feedback_repair_round"], 2);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 1);
    Ok(())
}

#[test]
fn local_review_pass_preserves_feedback_repair_history_until_remote_convergence(
) -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "local_review_gate",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "feedback_repair_round": 2,
        "feedback_repair_blocker_count": 1,
    }));
    let local_pass = WorkflowDecision::new(
        &instance.id,
        "local_review_gate",
        "local_review_passed",
        "awaiting_feedback",
        "Local review found no remaining blockers.",
    );
    let local_pass_event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::LOCAL_REVIEW_ACTIVITY,
                "signals": [{
                    "signal_type": crate::runtime::LOCAL_REVIEW_PASSED_SIGNAL,
                    "signal": {
                        "pr_number": 77,
                        "actionable_blocker_count": 0
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &local_pass, &local_pass_event)?;

    assert_eq!(instance.data["feedback_repair_round"], 2);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 1);
    Ok(())
}

#[test]
fn blocked_feedback_sweep_persists_the_observed_snapshot() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "feedback_repair_round": 2,
        "feedback_repair_blocker_count": 3,
    }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "block_feedback_repair_oscillation",
        "blocked",
        "Feedback repair did not reduce actionable blockers.",
    );
    let snapshot = json!({
        "snapshot_source": "server_github_graphql",
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "actionable_blocker_count": 2,
        "status_check_rollup_state": "SUCCESS",
        "observed_at": "2026-08-15T00:00:00Z"
    });
    let expected_hash = crate::runtime::stable_remote_fact_hash(
        &crate::runtime::stable_pr_snapshot_fact_hash_input(&snapshot),
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": "sweep_pr_feedback",
                "artifacts": [{
                    "artifact_type": crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
                    "artifact": snapshot
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["remote_fact_hash"], expected_hash);
    assert_eq!(instance.data["feedback_repair_round"], 2);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 3);
    Ok(())
}

#[test]
fn snapshotless_first_feedback_sweep_records_an_unmeasured_round() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({ "pr_number": 77 }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "address_pr_feedback",
        "addressing_feedback",
        "FeedbackFound signal requested repair without a snapshot.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": "sweep_pr_feedback",
                "signals": [{
                    "signal_type": "FeedbackFound",
                    "signal": { "pr_number": 77 }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert!(instance.data.get("feedback_repair_blocker_count").is_none());
    Ok(())
}

#[test]
fn snapshotless_feedback_sweep_persists_a_measured_signal_baseline() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        WorkflowSubject::new("issue", "77"),
    )
    .with_server_data(json!({ "pr_number": 77 }));
    let decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "address_pr_feedback",
        "addressing_feedback",
        "Measured feedback requested repair without a snapshot.",
    );
    let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-1")
        .with_payload(json!({
            "activity_result": {
                "activity": crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "signals": [{
                    "signal_type": "FeedbackFound",
                    "signal": {
                        "pr_number": 77,
                        "actionable_blocker_count": 2
                    }
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["feedback_repair_round"], 1);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 2);
    Ok(())
}

#[test]
fn completion_continuation_is_persisted_as_agent_data() -> anyhow::Result<()> {
    let policy = PromptContinuationPolicy {
        max_attempts: 3,
        attempt_delay_secs: 0,
        active_states: BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 2,
    };
    let continuation = PromptContinuationState::initial(&policy);
    let mut instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "continuation-agent"),
    )
    .with_classified_data(
        json!({ "prompt_ref": "prompt-ref" }),
        DataProvenance::Server,
    );

    persist_prompt_continuation(&mut instance, continuation)?;

    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|sidecar| sidecar.provenance_for("/continuation")),
        Some(DataProvenance::Agent)
    );
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_decision_persists_blocked_without_current_definition(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety.db")).await?;
    let instance = pin_error_instance("pin-safety-accepted");
    assert!(matches!(
        store
            .definition_registry()
            .resolve_declarative_definition(&instance),
        crate::runtime::state_registry::DeclarativeDefinitionResolution::PinError(
            crate::runtime::state_registry::DeclarativeDefinitionPinError::MissingVersion
        )
    ));
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let record = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-system",
            json!({}),
            &pin_safety_decision(&instance),
        )
        .await?
        .expect("decision should be recorded");
    assert!(record.accepted);
    assert_eq!(
        store
            .get_instance(&instance.id)
            .await?
            .expect("instance")
            .state,
        "blocked"
    );
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_decision_requires_explicit_context_override() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety-context.db")).await?;
    let instance = pin_error_instance("pin-safety-context-rejected");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let mut tx = store.pool.begin().await?;
    let event = insert_event_tx(
        &mut tx,
        &instance.id,
        "RuntimeJobCompleted",
        "runtime-system",
        json!({}),
    )
    .await?;
    let record = persist_runtime_completion_decision_with_context_tx(
        &mut tx,
        store.definition_registry(),
        instance.clone(),
        &event,
        pin_safety_decision(&instance),
        ValidationContext::new("runtime-system", event.created_at),
    )
    .await?;
    tx.commit().await?;

    assert!(!record.accepted);
    assert!(record
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("invalid declarative definition pin")));
    let persisted = store
        .get_instance(&instance.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("instance should remain present"))?;
    assert_eq!(persisted.state, "running");
    Ok(())
}

#[tokio::test]
async fn pin_error_safety_channel_rejects_any_extra_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("pin-safety-rejected.db")).await?;
    let instance = pin_error_instance("pin-safety-rejected");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let decision = pin_safety_decision(&instance)
        .with_command(WorkflowCommand::wait("not allowed", "pin:extra"));
    let record = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-system",
            json!({}),
            &decision,
        )
        .await?
        .expect("decision should be recorded");
    assert!(!record.accepted);
    assert_eq!(
        store
            .get_instance(&instance.id)
            .await?
            .expect("instance")
            .state,
        "running"
    );
    Ok(())
}
