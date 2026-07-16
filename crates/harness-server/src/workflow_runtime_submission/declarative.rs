use super::{
    merge_last_decision, prompt_memory::prompt_ref_for_submission, TaskId,
    WorkflowSubmissionRuntimeRecord, EXECUTION_PATH_WORKFLOW_RUNTIME,
};
use harness_workflow::runtime::{
    build_declarative_submission_decision, current_declarative_workflow_definition,
    decision_validator_for_instance, persisted_declarative_definition,
    DeclarativeWorkflowDefinition, ValidationContext, WorkflowCommandStatus, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject, WorkflowSubmissionDecisionTransition,
    WorkflowSubmissionPromptPayload, DECLARATIVE_SUBMISSION_DECISION,
};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

pub(crate) struct DeclarativeSubmissionRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub definition_id: &'a str,
    pub task_id: &'a TaskId,
    pub prompt: &'a str,
    pub depends_on: &'a [TaskId],
    pub serialization_depends_on: &'a [TaskId],
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
}

pub(crate) async fn record_declarative_submission(
    store: &WorkflowRuntimeStore,
    ctx: DeclarativeSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    if !ctx.depends_on.is_empty() || !ctx.serialization_depends_on.is_empty() {
        anyhow::bail!(
            "declarative workflow '{}' does not support dependencies",
            ctx.definition_id
        );
    }
    let definition =
        resolve_declarative_definition_for_project(ctx.project_root, ctx.definition_id)?;

    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        declarative_workflow_id(&project_id, ctx.definition_id, ctx.external_id, ctx.task_id);
    persist_definition_metadata(store, ctx.project_root, &definition).await?;
    if let Some(instance) = store.get_instance(&workflow_id).await? {
        return existing_submission(store, instance, &definition).await;
    }

    persist_new_submission(store, &ctx, &project_id, workflow_id, &definition).await
}

pub(crate) fn resolve_declarative_definition_for_project(
    project_root: &Path,
    definition_id: &str,
) -> anyhow::Result<Arc<DeclarativeWorkflowDefinition>> {
    let registered = current_declarative_workflow_definition(definition_id).ok_or_else(|| {
        anyhow::anyhow!(
            "workflow definition '{}' is not a registered declarative definition",
            definition_id
        )
    })?;
    let document = harness_core::config::workflow::load_workflow_document(project_root)?;
    let policy = document.config.definition.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "project '{}' does not declare workflow definition '{}'",
            project_root.display(),
            definition_id
        )
    })?;
    if policy.id != definition_id {
        anyhow::bail!(
            "project '{}' declares workflow definition '{}', not '{}'",
            project_root.display(),
            policy.id,
            definition_id
        );
    }
    let project_definition = harness_workflow::runtime::build_declarative_definition(
        policy,
        &document.config.activities,
    )?;
    if project_definition.definition_version() != registered.definition_version()
        || project_definition.definition_hash() != registered.definition_hash()
    {
        anyhow::bail!(
            "project '{}' workflow definition '{}' does not match the registered startup version",
            project_root.display(),
            definition_id
        );
    }
    Ok(registered)
}

pub(crate) fn declarative_workflow_id(
    project_id: &str,
    definition_id: &str,
    external_id: Option<&str>,
    task_id: &TaskId,
) -> String {
    format!(
        "{project_id}::declarative:{definition_id}:{}",
        subject_key(external_id, task_id)
    )
}

async fn persist_new_submission(
    store: &WorkflowRuntimeStore,
    ctx: &DeclarativeSubmissionRuntimeContext<'_>,
    project_id: &str,
    workflow_id: String,
    definition: &DeclarativeWorkflowDefinition,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let prompt_ref =
        prompt_ref_for_submission(project_id, ctx.external_id, ctx.task_id, ctx.prompt);
    let instance = submission_instance(ctx, project_id, &workflow_id, &prompt_ref, definition);
    let decision = build_declarative_submission_decision(definition, &instance)?;
    let validator = decision_validator_for_instance(&instance)
        .map_err(|error| {
            anyhow::anyhow!(
                "declarative workflow '{}@{}' has an invalid definition pin: {error:?}",
                definition.policy().id,
                definition.definition_version()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}@{}' has no registered decision validator",
                definition.policy().id,
                definition.definition_version()
            )
        })?;
    validator
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow-policy", chrono::Utc::now()),
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let mut final_instance = instance.clone();
    final_instance.state = decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = merge_last_decision(final_instance.data, &decision.decision);
    let event_id = uuid::Uuid::new_v4().to_string();
    let Some(outcome) = store
        .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
            workflow_id: &workflow_id,
            expected_state: &instance.state,
            expected_version: instance.version,
            create_if_missing: Some(&instance),
            event_id: None,
            new_event_id: Some(&event_id),
            event_type: "DeclarativeWorkflowSubmitted",
            source: "workflow_runtime_submission",
            payload: submission_event_payload(ctx),
            decision: &decision,
            existing_record: None,
            rejection_reason: None,
            final_instance: Some(&final_instance),
            command_status: WorkflowCommandStatus::Pending,
            prompt_payload: Some(WorkflowSubmissionPromptPayload {
                prompt_ref: &prompt_ref,
                prompt: ctx.prompt,
                previous_prompt_ref: None,
            }),
        })
        .await?
    else {
        let instance = store.get_instance(&workflow_id).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow submission '{}' disappeared during decision commit",
                workflow_id
            )
        })?;
        return existing_submission(store, instance, definition).await;
    };
    super::prompt_memory::cache_prompt_submission_prompt(&prompt_ref, ctx.prompt);
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id,
        accepted: true,
        decision_id: outcome.record.id,
        command_ids: outcome.command_ids,
        rejection_reason: None,
    })
}

pub(super) fn submission_instance(
    ctx: &DeclarativeSubmissionRuntimeContext<'_>,
    project_id: &str,
    workflow_id: &str,
    prompt_ref: &str,
    definition: &DeclarativeWorkflowDefinition,
) -> WorkflowInstance {
    let data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
            "project_id": project_id,
            "definition_hash": definition.definition_hash(),
            "submission_id": ctx.task_id.as_str(),
            "task_id": ctx.task_id.as_str(),
            "task_ids": [ctx.task_id.as_str()],
            "prompt_summary": "declarative workflow task",
            "prompt_chars": ctx.prompt.chars().count(),
            "prompt_ref": prompt_ref,
            "source": ctx.source,
            "external_id": ctx.external_id,
            "depends_on": [],
        }),
    );
    WorkflowInstance::new(
        definition.policy().id.as_str(),
        definition.definition_version(),
        "__submission__",
        WorkflowSubject::new("declarative", subject_key(ctx.external_id, ctx.task_id)),
    )
    .with_id(workflow_id)
    .with_data(data)
}

async fn persist_definition_metadata(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    definition: &DeclarativeWorkflowDefinition,
) -> anyhow::Result<()> {
    let document = harness_core::config::workflow::load_workflow_document(project_root)?;
    let persisted = persisted_declarative_definition(definition, document.source_path.as_deref());
    store.persist_definition_version(&persisted).await
}

async fn existing_submission(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    definition: &DeclarativeWorkflowDefinition,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    if instance.definition_id != definition.policy().id
        || instance.definition_version != definition.definition_version()
        || instance
            .data
            .get("definition_hash")
            .and_then(serde_json::Value::as_str)
            != Some(definition.definition_hash())
    {
        anyhow::bail!(
            "existing declarative workflow submission '{}' does not match the current pinned definition",
            instance.id
        );
    }
    let record = store
        .decisions_for(&instance.id)
        .await?
        .into_iter()
        .find(|record| {
            record.accepted && record.decision.decision == DECLARATIVE_SUBMISSION_DECISION
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "existing declarative workflow submission '{}' has no accepted submission decision",
                instance.id
            )
        })?;
    let command_ids = store
        .commands_for(&instance.id)
        .await?
        .into_iter()
        .filter(|command| command.decision_id.as_deref() == Some(record.id.as_str()))
        .map(|command| command.id)
        .collect();
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: instance.id,
        accepted: true,
        decision_id: record.id,
        command_ids,
        rejection_reason: None,
    })
}

fn subject_key(external_id: Option<&str>, task_id: &TaskId) -> String {
    external_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| task_id.as_str())
        .to_string()
}

fn submission_event_payload(ctx: &DeclarativeSubmissionRuntimeContext<'_>) -> serde_json::Value {
    json!({
        "task_id": ctx.task_id.as_str(),
        "definition_id": ctx.definition_id,
        "prompt_chars": ctx.prompt.chars().count(),
        "source": ctx.source,
        "external_id": ctx.external_id,
        "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
    })
}
