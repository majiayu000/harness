use super::{
    apply_inline_command_side_effect, command_store, insert_decision_record_tx, insert_event_tx,
    select_instance_for_update_tx, upsert_instance_tx, WorkflowRuntimeStore,
};
use crate::runtime::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::prompt_task::{
    prompt_continuation_state_from_data, PromptContinuationState, PROMPT_TASK_DEFINITION_ID,
};
use crate::runtime::reducer::{
    invalid_agent_output_blocked_decision, reduce_runtime_job_completed,
};
use crate::runtime::state_registry::{
    resolve_declarative_definition, DeclarativeDefinitionResolution,
};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::validator::{
    ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
use anyhow::Context;
use serde_json::{json, Value};

pub(super) fn validator_for_instance(
    instance: &WorkflowInstance,
) -> anyhow::Result<Option<crate::runtime::validator::DecisionValidator>> {
    crate::runtime::state_registry::decision_validator_for_instance(instance).map_err(|error| {
        anyhow::anyhow!(
            "invalid declarative definition pin for workflow '{}': {:?}",
            instance.id,
            error
        )
    })
}

impl WorkflowRuntimeStore {
    pub async fn commit_parent_runtime_completion(
        &self,
        parent_workflow_id: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let mut tx = self.pool.begin().await?;
        let Some(instance) = select_instance_for_update_tx(&mut tx, parent_workflow_id).await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        // Keep the same lock order as workflow transitions: instance row first,
        // workflow event sequence lock second.
        let event = insert_event_tx(
            &mut tx,
            parent_workflow_id,
            "RuntimeJobCompleted",
            source,
            payload,
        )
        .await?;
        let decision =
            apply_runtime_completion_decision_for_instance_tx(&mut tx, instance, source, &event)
                .await?;
        tx.commit().await?;
        if let Some(decision) = decision.as_ref() {
            self.record_terminal_repo_memory_for_completion(&event, decision)
                .await;
        }
        Ok(decision)
    }

    #[cfg(test)]
    pub(crate) async fn commit_runtime_completion_decision_for_test(
        &self,
        workflow_id: &str,
        source: &str,
        payload: Value,
        decision: &crate::runtime::model::WorkflowDecision,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let mut tx = self.pool.begin().await?;
        let Some(instance) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let event =
            insert_event_tx(&mut tx, workflow_id, "RuntimeJobCompleted", source, payload).await?;
        let record = persist_runtime_completion_decision_tx(
            &mut tx,
            instance,
            source,
            &event,
            decision.clone(),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(record))
    }
}

pub(super) async fn apply_runtime_completion_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    let Some(instance) = select_instance_for_update_tx(tx, workflow_id).await? else {
        return Ok(None);
    };
    apply_runtime_completion_decision_for_instance_tx(tx, instance, source, event).await
}

async fn apply_runtime_completion_decision_for_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    let driverless_decision = driverless_structured_completion_decision(&instance, source, event)?;
    let Some(decision) = reduce_runtime_job_completed(&instance, event)? else {
        return Ok(None);
    };
    // Preserve the requested liveness rejection only when the reducer found no
    // authoritative domain outcome and would apply its generic invalid-output policy.
    // The separate blocked policy decision remains the committed outcome, so the
    // completed driver cannot leave the workflow in an unowned progress state.
    if is_generic_invalid_structured_fallback(&decision) {
        if let Some(driverless_decision) = driverless_decision {
            let rejected = persist_runtime_completion_decision_tx(
                tx,
                instance.clone(),
                source,
                event,
                driverless_decision,
            )
            .await?;
            if rejected.accepted {
                anyhow::bail!(
                    "driverless completion candidate unexpectedly passed progress validation"
                );
            }
            let rejection_reason = rejected
                .rejection_reason
                .as_deref()
                .unwrap_or("missing rejection reason");
            let policy_decision = decision.with_evidence(WorkflowEvidence::new(
                "rejected_workflow_decision",
                format!(
                    "Rejected decision record `{}` before applying blocked policy: {rejection_reason}",
                    rejected.id
                ),
            ));
            let policy = persist_runtime_completion_decision_tx(
                tx,
                instance,
                source,
                event,
                policy_decision,
            )
            .await?;
            if !policy.accepted {
                anyhow::bail!(
                    "blocked policy decision was rejected after driverless completion: {}",
                    policy
                        .rejection_reason
                        .as_deref()
                        .unwrap_or("missing rejection reason")
                );
            }
            return Ok(Some(policy));
        }
    }

    if declarative_decision_missing_required_evidence(&instance, source, event, &decision) {
        let rejected =
            persist_runtime_completion_decision_tx(tx, instance.clone(), source, event, decision)
                .await?;
        if rejected.accepted {
            anyhow::bail!("missing-evidence declarative decision unexpectedly passed validation");
        }
        let result: ActivityResult =
            serde_json::from_value(event.event.get("activity_result").cloned().ok_or_else(
                || anyhow::anyhow!("RuntimeJobCompleted event missing activity_result"),
            )?)?;
        let rejection_reason = rejected
            .rejection_reason
            .as_deref()
            .unwrap_or("missing rejection reason");
        let policy_decision = invalid_agent_output_blocked_decision(
            &instance,
            event,
            &result,
            &format!(
                "declarative transition was rejected for missing evidence: {rejection_reason}"
            ),
        )
        .with_evidence(WorkflowEvidence::new(
            "rejected_workflow_decision",
            format!(
                "Rejected decision record `{}` before applying blocked policy: {rejection_reason}",
                rejected.id
            ),
        ));
        let policy =
            persist_runtime_completion_decision_tx(tx, instance, source, event, policy_decision)
                .await?;
        if !policy.accepted {
            anyhow::bail!(
                "blocked policy decision was rejected after missing declarative evidence: {}",
                policy
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("missing rejection reason")
            );
        }
        return Ok(Some(policy));
    }

    persist_runtime_completion_decision_tx(tx, instance, source, event, decision)
        .await
        .map(Some)
}

fn declarative_decision_missing_required_evidence(
    instance: &WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
    decision: &WorkflowDecision,
) -> bool {
    if !matches!(
        resolve_declarative_definition(instance),
        DeclarativeDefinitionResolution::Resolved(_)
    ) {
        return false;
    }
    let Ok(Some(validator)) = validator_for_instance(instance) else {
        return false;
    };
    matches!(
        validator.validate(
            instance,
            decision,
            &ValidationContext::new(source, event.created_at),
        ),
        Err(error) if error.kind == WorkflowDecisionRejectionKind::MissingRequiredEvidence
    )
}

fn is_generic_invalid_structured_fallback(decision: &WorkflowDecision) -> bool {
    decision.decision == "block_invalid_agent_output"
        && decision
            .reason
            .contains("did not validate and no domain fallback was available")
}

fn driverless_structured_completion_decision(
    instance: &WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecision>> {
    let Some(result) = event.event.get("activity_result") else {
        return Ok(None);
    };
    let result: ActivityResult = serde_json::from_value(result.clone())?;
    if result.status != ActivityStatus::Succeeded {
        return Ok(None);
    }
    let Some(decision) = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "workflow_decision")
        .find_map(|artifact| {
            serde_json::from_value::<WorkflowDecision>(artifact.artifact.clone()).ok()
        })
    else {
        return Ok(None);
    };
    let Ok(Some(validator)) = validator_for_instance(instance) else {
        return Ok(None);
    };
    let rejection = validator.validate(
        instance,
        &decision,
        &ValidationContext::new(source, event.created_at),
    );
    match rejection {
        Err(error) if error.kind == WorkflowDecisionRejectionKind::ProgressDriverMissing => {
            Ok(Some(decision))
        }
        _ => Ok(None),
    }
}

async fn persist_runtime_completion_decision_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
    decision: WorkflowDecision,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let validation_context = runtime_completion_validation_context(source, event);
    persist_runtime_completion_decision_with_context_tx(
        tx,
        instance,
        event,
        decision,
        validation_context,
    )
    .await
}

async fn persist_runtime_completion_decision_with_context_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mut instance: WorkflowInstance,
    event: &WorkflowEvent,
    decision: WorkflowDecision,
    validation_context: ValidationContext,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let record = match validator_for_instance(&instance) {
        Ok(Some(validator)) => {
            match validator.validate(&instance, &decision, &validation_context) {
                Ok(()) => {
                    WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()))
                }
                Err(error) => WorkflowDecisionRecord::rejected(
                    decision,
                    Some(event.id.clone()),
                    error.to_string(),
                ),
            }
        }
        Ok(None) => WorkflowDecisionRecord::rejected(
            decision,
            Some(event.id.clone()),
            "unknown workflow definition for runtime completion",
        ),
        Err(_error) if validation_context.allow_definition_pin_safety_decision => {
            match validate_definition_pin_safety_transition(
                &instance,
                &decision,
                &validation_context,
            ) {
                Ok(()) => {
                    WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()))
                }
                Err(error) => WorkflowDecisionRecord::rejected(
                    decision,
                    Some(event.id.clone()),
                    error.to_string(),
                ),
            }
        }
        Err(error) => {
            WorkflowDecisionRecord::rejected(decision, Some(event.id.clone()), error.to_string())
        }
    };
    insert_decision_record_tx(tx, &record).await?;

    if record.accepted {
        if apply_prompt_continuation_side_effect(tx, &mut instance, &record.decision).await?
            == PromptContinuationSideEffect::DurableReplay
        {
            return Ok(record);
        }
        for followup in &record.decision.commands {
            let status = if followup.requires_runtime_job() {
                WorkflowCommandStatus::Pending
            } else {
                WorkflowCommandStatus::HandledInline
            };
            command_store::insert_tx(tx, &instance.id, Some(&record.id), followup, status).await?;
            if !followup.requires_runtime_job() {
                apply_inline_command_side_effect(&mut instance, followup)?;
            }
        }
        apply_runtime_completion_data_side_effect(&mut instance, &record.decision, event)?;
        instance.state = record.decision.next_state.clone();
        instance.version = instance.version.saturating_add(1);
        upsert_instance_tx(tx, &instance).await?;
    }

    Ok(record)
}

fn runtime_completion_validation_context(source: &str, event: &WorkflowEvent) -> ValidationContext {
    ValidationContext::new(source, event.created_at).allow_definition_pin_safety_decision()
}

fn apply_runtime_completion_data_side_effect(
    instance: &mut WorkflowInstance,
    decision: &WorkflowDecision,
    event: &WorkflowEvent,
) -> anyhow::Result<()> {
    if instance.definition_id != crate::runtime::PR_FEEDBACK_DEFINITION_ID
        || instance.state != "inspecting"
        || decision.observed_state != "inspecting"
        || !matches!(
            decision.next_state.as_str(),
            "feedback_found" | "no_actionable_feedback" | "ready_to_merge"
        )
    {
        return Ok(());
    }
    let Some(snapshot) = pr_feedback_snapshot_from_completion_event(event) else {
        return Ok(());
    };
    let facts_for_hash = crate::runtime::stable_pr_snapshot_fact_hash_input(snapshot);
    let fact_hash = crate::runtime::stable_remote_fact_hash(&facts_for_hash);
    let activity_at = ["updated_at", "updatedAt"].into_iter().find_map(|field| {
        snapshot
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("remote_fact_hash".to_string(), json!(fact_hash));
    if let Some(activity_at) = activity_at {
        data.insert("remote_fact_activity_at".to_string(), json!(activity_at));
    } else {
        data.remove("remote_fact_activity_at");
    }
    Ok(())
}

fn pr_feedback_snapshot_from_completion_event(event: &WorkflowEvent) -> Option<&Value> {
    let result = event.event.get("activity_result")?;
    if result.get("activity").and_then(Value::as_str)
        != Some(crate::runtime::PR_FEEDBACK_INSPECT_ACTIVITY)
    {
        return None;
    }
    result
        .get("artifacts")
        .and_then(Value::as_array)?
        .iter()
        .find(|artifact| {
            matches!(
                artifact.get("artifact_type").and_then(Value::as_str),
                Some(crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT)
                    | Some(crate::runtime::PR_FEEDBACK_SNAPSHOT_ARTIFACT)
            )
        })
        .and_then(|artifact| artifact.get("artifact"))
}

fn validate_definition_pin_safety_transition(
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if !context.allow_definition_pin_safety_decision {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            "definition-pin safety transition requires explicit validation context",
        ));
    }
    if context.actor.trim().is_empty()
        || decision.workflow_id != instance.id
        || decision.observed_state != instance.state
        || decision.next_state != "blocked"
        || decision.decision != "definition_version_missing"
        || decision.commands.len() != 2
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            "definition-pin safety transition does not match the pinned-definition blocked policy",
        ));
    }
    let expected = [
        crate::runtime::model::WorkflowCommandType::MarkBlocked,
        crate::runtime::model::WorkflowCommandType::RequestOperatorAttention,
    ];
    if !decision
        .commands
        .iter()
        .zip(expected)
        .all(|(command, command_type)| {
            command.command_type == command_type
                && !command.dedupe_key.trim().is_empty()
                && command.command.is_object()
        })
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::CommandNotAllowed,
            "definition-pin safety transition has invalid commands",
        ));
    }
    if !decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "definition_pin_error")
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::MissingRequiredEvidence,
            "definition-pin safety transition requires definition_pin_error evidence",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptContinuationSideEffect {
    Applied,
    DurableReplay,
}

#[cfg(test)]
#[path = "runtime_completion_tests.rs"]
mod tests;

async fn apply_prompt_continuation_side_effect(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &mut WorkflowInstance,
    decision: &WorkflowDecision,
) -> anyhow::Result<PromptContinuationSideEffect> {
    let missing_completion_evidence_for_continuation = decision.decision
        == "prompt_completion_evidence_missing"
        && instance.data.get("continuation").is_some();
    if instance.definition_id != PROMPT_TASK_DEFINITION_ID
        || (!matches!(
            decision.decision.as_str(),
            "continue_prompt_task"
                | "finish_prompt_task_external_settled"
                | "prompt_continuation_exhausted"
                | "prompt_continuation_no_progress"
        ) && !missing_completion_evidence_for_continuation)
    {
        return Ok(PromptContinuationSideEffect::Applied);
    }
    let previous = prompt_continuation_state_from_data(&instance.data)
        .map_err(anyhow::Error::msg)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "prompt continuation decision `{}` requires authoritative persisted continuation state",
                decision.decision
            )
        })?;
    let continuation_values = decision
        .commands
        .iter()
        .filter_map(|command| command.command.get("continuation"))
        .collect::<Vec<_>>();
    if continuation_values.len() != 1 {
        anyhow::bail!(
            "prompt continuation decision `{}` must persist exactly one continuation state",
            decision.decision
        );
    }
    let continuation: PromptContinuationState =
        serde_json::from_value(continuation_values[0].clone())?;
    continuation.policy.validate()?;
    if continuation.policy != previous.policy {
        anyhow::bail!(
            "prompt continuation decision `{}` cannot replace the persisted policy",
            decision.decision
        );
    }
    if decision.decision == "continue_prompt_task" && continuation == previous {
        require_durable_continuation_replay(tx, instance, decision).await?;
        return Ok(PromptContinuationSideEffect::DurableReplay);
    }
    let expected_attempt = if decision.decision == "continue_prompt_task" {
        previous.attempt.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("prompt continuation attempt overflowed the persisted counter")
        })?
    } else {
        previous.attempt
    };
    if continuation.attempt != expected_attempt {
        anyhow::bail!(
            "prompt continuation decision `{}` expected attempt {}, got {}",
            decision.decision,
            expected_attempt,
            continuation.attempt
        );
    }
    if continuation.attempt == 0 || continuation.attempt > continuation.policy.max_attempts {
        anyhow::bail!(
            "prompt continuation decision `{}` has invalid attempt {}",
            decision.decision,
            continuation.attempt
        );
    }
    let data = instance
        .data
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("prompt task instance data must be a JSON object"))?;
    data.insert(
        "continuation".to_string(),
        serde_json::to_value(continuation)?,
    );
    Ok(PromptContinuationSideEffect::Applied)
}

async fn require_durable_continuation_replay(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
) -> anyhow::Result<()> {
    let command = decision.commands.first().ok_or_else(|| {
        anyhow::anyhow!("prompt continuation replay requires its attempt-scoped command")
    })?;
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM workflow_commands
         WHERE workflow_id = $1 AND dedupe_key = $2",
    )
    .bind(&instance.id)
    .bind(&command.dedupe_key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((data,)) = existing else {
        anyhow::bail!(
            "prompt continuation replay is missing durable command `{}`",
            command.dedupe_key
        );
    };
    let existing_command: WorkflowCommand = serde_json::from_str(&data)?;
    if existing_command != *command {
        anyhow::bail!(
            "prompt continuation replay command `{}` does not match durable state",
            command.dedupe_key
        );
    }
    Ok(())
}
