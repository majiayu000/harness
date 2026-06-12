use harness_workflow::runtime::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance, WorkflowRuntimeStore,
    PROMPT_TASK_DEFINITION_ID,
};
use serde_json::json;
use std::fmt;

use super::prompt_memory::remove_prompt_submission_prompt_durable;
use super::{
    commit_runtime_decision, optional_string_field, runtime_issue_task_handle, set_data_bool,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};

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

pub(crate) async fn cancel_issue_submission_by_submission_id(
    store: &WorkflowRuntimeStore,
    submission_id: &crate::task_runner::TaskId,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    let Some(instance) = store
        .get_instance_by_submission_id(submission_id.as_str())
        .await?
    else {
        return Ok(RuntimeSubmissionCancelOutcome::NotFound);
    };
    cancel_submission_instance(store, instance, submission_id.as_str()).await
}

pub(crate) async fn cancel_issue_submission_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &crate::task_runner::TaskId,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    cancel_issue_submission_by_submission_id(store, task_id).await
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
    instance: WorkflowInstance,
    correlation_id: &str,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    if instance.is_terminal() {
        return Ok(RuntimeSubmissionCancelOutcome::AlreadyTerminal(instance));
    }
    let is_prompt = instance.definition_id == PROMPT_TASK_DEFINITION_ID;
    if !is_prompt && instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return Err(RuntimeSubmissionCancelError::UnsupportedDefinition {
            definition_id: instance.definition_id,
        });
    }
    let event_type = if is_prompt {
        "PromptSubmissionCancelled"
    } else {
        "IssueSubmissionCancelled"
    };
    let decision_name = if is_prompt {
        "cancel_prompt_submission"
    } else {
        "cancel_issue_submission"
    };
    let reason = if is_prompt {
        "operator cancelled the runtime prompt submission"
    } else {
        "operator cancelled the runtime issue submission"
    };
    let command_prefix = if is_prompt {
        "prompt-submit"
    } else {
        "issue-submit"
    };
    let event = store
        .append_event(
            &instance.id,
            event_type,
            "workflow_runtime_submission",
            json!({
                "task_id": correlation_id,
                "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "cancelled",
        reason,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!("{command_prefix}:{correlation_id}:cancel"),
        json!({ "task_id": correlation_id }),
    ))
    .high_confidence();
    let mut cancelled = commit_runtime_decision(store, instance, decision, event.id, None).await?;
    let commands = store.commands_for(&cancelled.id).await?;
    for command in commands {
        if matches!(
            command.status.as_str(),
            "pending" | "dispatching" | "dispatched"
        ) {
            store
                .cancel_command_and_unfinished_runtime_jobs(
                    &command.id,
                    decision_name,
                    "Runtime submission was cancelled before execution.",
                )
                .await?;
        }
    }
    cancelled.data = set_data_bool(cancelled.data, "cancelled", true);
    store.upsert_instance(&cancelled).await?;
    if is_prompt {
        remove_prompt_submission_prompt_durable(
            store,
            optional_string_field(&cancelled.data, "prompt_ref").as_deref(),
        )
        .await?;
    }
    Ok(RuntimeSubmissionCancelOutcome::Cancelled(cancelled))
}
