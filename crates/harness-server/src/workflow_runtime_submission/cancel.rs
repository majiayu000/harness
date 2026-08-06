use harness_core::config::workflow::{WorkflowActivityPolicy, WorkflowDefinitionPolicy};
use harness_workflow::runtime::{
    build_declarative_definition, resolve_declarative_definition, DecisionValidator,
    DeclarativeDefinitionResolution, WorkflowCancellationCleanupOutcome, WorkflowCommand,
    WorkflowCommandType, WorkflowDecision, WorkflowInstance, WorkflowRuntimeStore,
    WorkflowTerminalState, PROMPT_TASK_DEFINITION_ID,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt;

use super::prompt_memory::remove_prompt_submission_prompt_durable;
use super::{
    commit_runtime_decision, commit_runtime_decision_with_validator, optional_string_field,
    runtime_issue_task_handle, GITHUB_ISSUE_PR_DEFINITION_ID,
};

struct DeclarativeCancellation {
    target_state: String,
    validator: DecisionValidator,
    missing_pin: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum RuntimeSubmissionCancelOutcome {
    Cancelled(WorkflowInstance),
    AlreadyTerminal(WorkflowInstance),
    NotFound,
}

#[derive(Debug)]
pub(crate) enum RuntimeSubmissionCancelError {
    UnsupportedDefinition { definition_id: String },
    Store(anyhow::Error),
}

impl fmt::Display for RuntimeSubmissionCancelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDefinition { definition_id } => write!(
                formatter,
                "workflow definition `{definition_id}` cannot be cancelled as a runtime submission"
            ),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RuntimeSubmissionCancelError {}

impl From<anyhow::Error> for RuntimeSubmissionCancelError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
pub(crate) async fn cancel_issue_submission_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &crate::workflow_runtime_submission::TaskId,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    let Some(instance) = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
    else {
        return Ok(RuntimeSubmissionCancelOutcome::NotFound);
    };
    cancel_submission_instance(store, instance, task_id.as_str()).await
}

pub(crate) async fn cancel_submission_by_workflow_id(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(RuntimeSubmissionCancelOutcome::NotFound);
    };
    let correlation_id = runtime_issue_task_handle(&instance)
        .map(|task_id| task_id.0)
        .unwrap_or_else(|| format!("workflow:{workflow_id}"));
    cancel_submission_instance(store, instance, &correlation_id).await
}

async fn cancel_submission_instance(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    correlation_id: &str,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    if instance.is_terminal() {
        if instance.terminal_state() == Some(WorkflowTerminalState::Cancelled) {
            let (decision_name, remove_prompt) = cancellation_cleanup_policy(&instance);
            finish_cancellation_cleanup(store, &mut instance, decision_name, remove_prompt).await?;
        }
        return Ok(RuntimeSubmissionCancelOutcome::AlreadyTerminal(instance));
    }
    let is_prompt = instance.definition_id == PROMPT_TASK_DEFINITION_ID;
    let is_issue = instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID;
    let declarative = if is_prompt || is_issue {
        None
    } else {
        resolve_declarative_cancellation(store, &instance).await?
    };
    let (event_type, decision_name, reason, command_prefix, target_state, remove_prompt) =
        if is_prompt {
            (
                "PromptSubmissionCancelled",
                "cancel_prompt_submission",
                "operator cancelled the runtime prompt submission",
                "prompt-submit",
                "cancelled".to_string(),
                true,
            )
        } else if is_issue {
            (
                "IssueSubmissionCancelled",
                "cancel_issue_submission",
                "operator cancelled the runtime issue submission",
                "issue-submit",
                "cancelled".to_string(),
                false,
            )
        } else {
            let declarative = declarative.as_ref().ok_or_else(|| {
                RuntimeSubmissionCancelError::UnsupportedDefinition {
                    definition_id: instance.definition_id.clone(),
                }
            })?;
            (
                "DeclarativeSubmissionCancelled",
                "cancel_declarative_submission",
                "operator cancelled the runtime declarative submission",
                "declarative-submit",
                declarative.target_state.clone(),
                true,
            )
        };
    let event_payload = json!({
        "task_id": correlation_id,
        "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        target_state,
        reason,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!("{command_prefix}:{correlation_id}:cancel"),
        json!({ "task_id": correlation_id }),
    ))
    .high_confidence();
    let mut cancelled = if let Some(declarative) = declarative {
        commit_runtime_decision_with_validator(
            store,
            instance,
            decision,
            event_type,
            "workflow_runtime_submission",
            event_payload,
            None,
            declarative.validator,
            declarative.missing_pin,
        )
        .await?
    } else {
        commit_runtime_decision(
            store,
            instance,
            decision,
            event_type,
            "workflow_runtime_submission",
            event_payload,
            None,
        )
        .await?
    };
    let cleaned =
        finish_cancellation_cleanup(store, &mut cancelled, decision_name, remove_prompt).await?;
    if !cleaned {
        return Err(RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
            "runtime submission cancellation did not persist a MarkCancelled command"
        )));
    }
    Ok(RuntimeSubmissionCancelOutcome::Cancelled(cancelled))
}

fn cancellation_cleanup_policy(instance: &WorkflowInstance) -> (&'static str, bool) {
    if instance.definition_id == PROMPT_TASK_DEFINITION_ID {
        ("cancel_prompt_submission", true)
    } else if instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID {
        ("cancel_issue_submission", false)
    } else {
        ("cancel_declarative_submission", true)
    }
}

async fn finish_cancellation_cleanup(
    store: &WorkflowRuntimeStore,
    cancelled: &mut WorkflowInstance,
    decision_name: &str,
    remove_prompt: bool,
) -> Result<bool, RuntimeSubmissionCancelError> {
    match store
        .finish_cancellation_cleanup_if_current(
            cancelled,
            decision_name,
            "Runtime submission was cancelled before execution.",
        )
        .await?
    {
        WorkflowCancellationCleanupOutcome::Cleaned(instance) => *cancelled = *instance,
        WorkflowCancellationCleanupOutcome::NoCancellationCommand => return Ok(false),
        WorkflowCancellationCleanupOutcome::StaleInstance => {
            return Err(RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
                "workflow changed before cancellation cleanup could be committed"
            )));
        }
    }
    if remove_prompt {
        remove_prompt_submission_prompt_durable(
            store,
            optional_string_field(&cancelled.data, "prompt_ref").as_deref(),
        )
        .await?;
    }
    Ok(true)
}

async fn resolve_declarative_cancellation(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> Result<Option<DeclarativeCancellation>, RuntimeSubmissionCancelError> {
    if let DeclarativeDefinitionResolution::Resolved(definition) =
        resolve_declarative_definition(instance)
    {
        let target_state = cancelled_state(definition.policy(), instance)?;
        return Ok(Some(DeclarativeCancellation {
            target_state,
            // The pin resolved to this exact definition, so the validator
            // carries that identity and the store can re-verify at commit that
            // it still governs the row it locked (GH-1864).
            validator: DecisionValidator::for_declarative_definition(
                &instance.definition_id,
                definition.definition_version(),
                definition.definition_hash(),
                definition.registered().allowlist.clone(),
            ),
            missing_pin: false,
        }));
    }

    let Some(persisted) = store
        .get_definition(&instance.definition_id, instance.definition_version)
        .await?
    else {
        return match resolve_declarative_definition(instance) {
            DeclarativeDefinitionResolution::PinError(error) => {
                Err(RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
                    "declarative workflow '{}' has an invalid definition pin and no persisted definition during cancellation: {error:?}",
                    instance.id
                )))
            }
            DeclarativeDefinitionResolution::NotDeclarative => Ok(None),
            DeclarativeDefinitionResolution::Resolved(_) => unreachable!(
                "resolved declarative cancellation returned before persisted lookup"
            ),
        };
    };
    if persisted
        .metadata
        .get("kind")
        .and_then(serde_json::Value::as_str)
        != Some("declarative_workflow")
    {
        return Ok(None);
    }
    let policy: WorkflowDefinitionPolicy =
        serde_json::from_value(persisted.metadata.get("policy").cloned().ok_or_else(|| {
            RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
                "persisted declarative workflow '{}@{}' is missing policy metadata",
                instance.definition_id,
                instance.definition_version
            ))
        })?)
        .map_err(anyhow::Error::from)?;
    let activity_policies = policy
        .states
        .values()
        .filter_map(|state| state.activity.as_ref())
        .map(|activity| (activity.clone(), WorkflowActivityPolicy::default()))
        .collect::<BTreeMap<_, _>>();
    let definition = build_declarative_definition(&policy, &activity_policies)?;
    let expected_hash = instance
        .data
        .get("definition_hash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
                "declarative workflow '{}' is missing its pinned definition hash",
                instance.id
            ))
        })?;
    if definition.definition_version() != instance.definition_version
        || definition.definition_hash() != expected_hash
        || persisted.definition_hash != expected_hash
    {
        return Err(RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
            "persisted declarative workflow '{}' does not match its pinned definition identity",
            instance.id
        )));
    }
    Ok(Some(DeclarativeCancellation {
        target_state: cancelled_state(&policy, instance)?,
        // Rebuilt from the persisted snapshot, but only after its version and
        // content hash were checked against the instance pin above, so the
        // validator may claim that identity (GH-1864).
        validator: DecisionValidator::for_declarative_definition(
            &instance.definition_id,
            definition.definition_version(),
            definition.definition_hash(),
            definition.registered().allowlist.clone(),
        ),
        missing_pin: true,
    }))
}

fn cancelled_state(
    policy: &WorkflowDefinitionPolicy,
    instance: &WorkflowInstance,
) -> Result<String, RuntimeSubmissionCancelError> {
    policy
        .terminal
        .iter()
        .find_map(|(state, class)| (class == "cancelled").then_some(state.clone()))
        .ok_or_else(|| {
            RuntimeSubmissionCancelError::Store(anyhow::anyhow!(
                "declarative workflow '{}' has no cancelled terminal state",
                instance.id
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use harness_core::db::resolve_database_url;
    use harness_workflow::runtime::{
        RuntimeJobStatus, RuntimeKind, WorkflowCommandStatus, WorkflowDecisionRecord,
        WorkflowSubject,
    };

    #[tokio::test]
    async fn terminal_cancellation_retry_finishes_command_and_job_cleanup() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let database_url = resolve_database_url(None)?;
        let store =
            WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
        let workflow = WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "cancelled",
            WorkflowSubject::new("prompt", "retry-cancellation-cleanup"),
        )
        .with_id("retry-cancellation-cleanup");
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &workflow)
            .await?;

        let activity = WorkflowCommand::enqueue_activity(
            "implement_prompt",
            "retry-cancellation-cleanup-activity",
        );
        let activity_command_id = store.enqueue_command(&workflow.id, None, &activity).await?;
        let runtime_job = store
            .enqueue_runtime_job(
                &activity_command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({"activity": "implement_prompt"}),
            )
            .await?;
        let Some(running_job) = store
            .claim_next_runtime_job("retry-cleanup", Utc::now() + Duration::minutes(5))
            .await?
        else {
            anyhow::bail!("runtime job should be running before cancellation retry");
        };
        assert_eq!(running_job.id, runtime_job.id);

        let cancellation = WorkflowCommand::new(
            WorkflowCommandType::MarkCancelled,
            "retry-cancellation-cleanup-marker",
            json!({}),
        );
        // Cleanup only honors a marker bound to the accepted cancellation
        // decision that minted it (GH-1865), so the fixture commits one.
        let cancellation_decision = WorkflowDecision::new(
            &workflow.id,
            "running",
            "cancel_prompt_submission",
            "cancelled",
            "operator cancelled the runtime prompt submission",
        )
        .with_command(cancellation.clone());
        let decision_record = WorkflowDecisionRecord::accepted(cancellation_decision, None);
        store.record_decision(&decision_record).await?;
        store
            .enqueue_command(&workflow.id, Some(&decision_record.id), &cancellation)
            .await?;

        let outcome = cancel_submission_by_workflow_id(&store, &workflow.id).await?;
        let RuntimeSubmissionCancelOutcome::AlreadyTerminal(first_returned) = outcome else {
            anyhow::bail!("terminal cancellation should remain terminal");
        };
        let Some(activity_command) = store.get_command(&activity_command_id).await? else {
            anyhow::bail!("activity command should remain queryable");
        };
        assert_eq!(activity_command.status, WorkflowCommandStatus::Cancelled);
        let Some(cancelled_job) = store.get_runtime_job(&runtime_job.id).await? else {
            anyhow::bail!("runtime job should remain queryable");
        };
        assert_eq!(cancelled_job.status, RuntimeJobStatus::Cancelled);
        let Some(mut stale_cancelled) = store.get_instance(&workflow.id).await? else {
            anyhow::bail!("cancelled workflow should remain queryable");
        };
        assert_eq!(stale_cancelled.data["cancelled"], true);
        assert_eq!(
            first_returned.version, stale_cancelled.version,
            "cleanup must return the persisted post-cleanup version"
        );
        let first_cleanup_version = stale_cancelled.version;

        let retry = cancel_submission_by_workflow_id(&store, &workflow.id).await?;
        let RuntimeSubmissionCancelOutcome::AlreadyTerminal(retry_returned) = retry else {
            anyhow::bail!("terminal cancellation retry should remain terminal");
        };
        let retry_stored = store
            .get_instance(&workflow.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("cancelled workflow disappeared after retry"))?;
        assert_eq!(retry_returned.version, first_cleanup_version);
        assert_eq!(
            retry_stored.version, first_cleanup_version,
            "an idempotent cancellation retry must not advance the instance version"
        );

        let mut reopened = stale_cancelled.clone();
        reopened.state = "planning".to_string();
        reopened.version += 1;
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &reopened)
            .await?;
        let reopened_command =
            WorkflowCommand::enqueue_activity("plan_prompt", "retry-cancellation-cleanup-reopened");
        let reopened_command_id = store
            .enqueue_command(&workflow.id, None, &reopened_command)
            .await?;

        let error = match finish_cancellation_cleanup(
            &store,
            &mut stale_cancelled,
            "cancel_prompt_submission",
            true,
        )
        .await
        {
            Ok(_) => panic!("stale cancellation cleanup should fail after reopen"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("workflow changed before cancellation cleanup"));
        let Some(reopened_command) = store.get_command(&reopened_command_id).await? else {
            anyhow::bail!("reopened workflow command should remain queryable");
        };
        assert_eq!(reopened_command.status, WorkflowCommandStatus::Pending);

        let mut completed = reopened;
        completed.state = "done".to_string();
        completed.version += 1;
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &completed)
            .await?;
        let outcome = cancel_submission_by_workflow_id(&store, &workflow.id).await?;
        assert!(matches!(
            outcome,
            RuntimeSubmissionCancelOutcome::AlreadyTerminal(_)
        ));
        let Some(completed_command) = store.get_command(&reopened_command_id).await? else {
            anyhow::bail!("completed generation command should remain queryable");
        };
        assert_eq!(completed_command.status, WorkflowCommandStatus::Pending);
        Ok(())
    }
}
