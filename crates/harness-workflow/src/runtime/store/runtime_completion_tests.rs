use super::*;
use crate::runtime::model::{WorkflowCommandType, WorkflowEvidence, WorkflowSubject};
use crate::runtime::{DataProvenance, PromptContinuationPolicy};
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::collections::BTreeSet;

#[test]
fn reads_scope_assessed_head_only_from_classifier_assessment() {
    let event = WorkflowEvent::new("workflow-1", 1, "RuntimeJobCompleted", "runtime")
        .with_payload(json!({
            "activity_result": {
                "artifacts": [{
                    "artifact_type": crate::runtime::completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT,
                    "artifact": {"subject_head_oid": "head-123"}
                }]
            }
        }));

    assert_eq!(
        classifier_assessed_head_from_completion_event(&event),
        Some("head-123")
    );
}

#[test]
fn persists_scope_assessed_head_on_pr_scope_approval() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_scope_review",
        WorkflowSubject::new("issue", "owner/repo#42"),
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        "pr_scope_review",
        "apply_declarative_transition",
        "pr_open",
        "scope classifier allowed this head",
    );
    let event = WorkflowEvent::new("workflow-1", 1, "RuntimeJobCompleted", "runtime")
        .with_payload(json!({
            "activity_result": {
                "artifacts": [{
                    "artifact_type": crate::runtime::completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT,
                    "artifact": {"subject_head_oid": "head-456"}
                }]
            }
        }));

    apply_runtime_completion_data_side_effect(&mut instance, &decision, &event)?;

    assert_eq!(instance.data["scope_assessed_head_oid"], "head-456");
    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|provenance| provenance.provenance_for("/scope_assessed_head_oid")),
        Some(DataProvenance::Server)
    );
    Ok(())
}

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
