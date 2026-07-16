use super::{
    apply_declarative_decision, prompt_ref_for_submission, task_id_history,
    WorkflowSubmissionRuntimeRecord, EXECUTION_PATH_WORKFLOW_RUNTIME,
};
use crate::task_runner::TaskId;
use anyhow::Context;
use harness_workflow::runtime::{
    build_declarative_definition, build_declarative_submission_decision,
    current_declarative_workflow_definition, persisted_declarative_definition,
    DeclarativeWorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    DECLARATIVE_SUBMISSION_DECISION,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub(crate) struct DeclarativeSubmissionRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub task_id: &'a TaskId,
    pub definition_id: &'a str,
    pub prompt: &'a str,
    pub depends_on: &'a [TaskId],
    pub serialization_depends_on: &'a [TaskId],
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
}

pub(crate) fn resolve_project_declarative_definition(
    project_root: &Path,
    definition_id: &str,
) -> anyhow::Result<(
    Arc<DeclarativeWorkflowDefinition>,
    harness_core::config::workflow::WorkflowDocument,
)> {
    let document = harness_core::config::workflow::load_workflow_document(project_root)
        .with_context(|| {
            format!(
                "failed to load WORKFLOW.md for declarative submission at '{}'",
                project_root.display()
            )
        })?;
    let policy = document.config.definition.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "project '{}' does not declare a workflow definition",
            project_root.display()
        )
    })?;
    if policy.id != definition_id {
        anyhow::bail!(
            "project '{}' declares workflow definition '{}', not requested definition '{}'",
            project_root.display(),
            policy.id,
            definition_id
        );
    }
    let compiled = build_declarative_definition(policy, &document.config.activities)?;
    let registered = current_declarative_workflow_definition(definition_id).ok_or_else(|| {
        anyhow::anyhow!(
            "declarative workflow definition '{}' was not registered at server startup",
            definition_id
        )
    })?;
    if compiled.definition_version() != registered.definition_version()
        || compiled.definition_hash() != registered.definition_hash()
    {
        anyhow::bail!(
            "project '{}' WORKFLOW.md definition '{}' changed after server startup; restart Harness before submitting new instances",
            project_root.display(),
            definition_id
        );
    }
    Ok((registered, document))
}

pub(crate) async fn record_declarative_submission(
    store: &WorkflowRuntimeStore,
    ctx: DeclarativeSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    if !ctx.depends_on.is_empty() || !ctx.serialization_depends_on.is_empty() {
        anyhow::bail!(
            "declarative workflow '{}' does not support depends_on in v1",
            ctx.definition_id
        );
    }
    let (definition, document) =
        resolve_project_declarative_definition(ctx.project_root, ctx.definition_id)?;
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        declarative_workflow_id(&project_id, ctx.definition_id, ctx.external_id, ctx.task_id);
    store
        .persist_definition_version(&persisted_declarative_definition(
            &definition,
            document.source_path.as_deref(),
        ))
        .await?;
    if store.get_instance(&workflow_id).await?.is_some() {
        return existing_declarative_submission(
            store,
            &workflow_id,
            definition.policy().id.as_str(),
            definition.definition_version(),
            definition.definition_hash(),
        )
        .await;
    }
    let instance = declarative_instance(&definition, workflow_id, &project_id, &ctx);
    let prompt_ref =
        prompt_ref_for_submission(&project_id, ctx.external_id, ctx.task_id, ctx.prompt);
    let submitted_data =
        declarative_submission_data(&instance.data, &project_id, &prompt_ref, &definition, &ctx);
    let decision = build_declarative_submission_decision(&definition, &instance)?;
    apply_declarative_decision(store, instance, true, decision, &ctx, submitted_data).await
}

pub(super) async fn existing_declarative_submission(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    definition_id: &str,
    definition_version: u32,
    definition_hash: &str,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let instance = store.get_instance(workflow_id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "declarative workflow submission '{}' disappeared during conflict reconciliation",
            workflow_id
        )
    })?;
    if instance.definition_id != definition_id
        || instance.definition_version != definition_version
        || instance.data.get("definition_hash").and_then(Value::as_str) != Some(definition_hash)
    {
        anyhow::bail!(
            "existing declarative workflow submission '{}' does not match definition '{}@{}'",
            workflow_id,
            definition_id,
            definition_version
        );
    }
    let record = store
        .decisions_for(workflow_id)
        .await?
        .into_iter()
        .find(|record| {
            record.accepted && record.decision.decision == DECLARATIVE_SUBMISSION_DECISION
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "existing declarative workflow submission '{}' has no accepted submission decision",
                workflow_id
            )
        })?;
    let command_ids = store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .filter(|command| command.decision_id.as_deref() == Some(record.id.as_str()))
        .map(|command| command.id)
        .collect();
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: workflow_id.to_string(),
        accepted: true,
        decision_id: record.id,
        command_ids,
        rejection_reason: None,
    })
}

pub(crate) fn declarative_workflow_id(
    project_id: &str,
    definition_id: &str,
    external_id: Option<&str>,
    task_id: &TaskId,
) -> String {
    let subject = external_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| task_id.as_str());
    format!("{project_id}::declarative:{definition_id}:{subject}")
}

fn declarative_instance(
    definition: &DeclarativeWorkflowDefinition,
    workflow_id: String,
    project_id: &str,
    ctx: &DeclarativeSubmissionRuntimeContext<'_>,
) -> WorkflowInstance {
    let subject_key = ctx
        .external_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ctx.task_id.as_str());
    WorkflowInstance::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        definition.policy().initial.clone(),
        WorkflowSubject::new("declarative", subject_key),
    )
    .with_id(workflow_id)
    .with_data(json!({
        "definition_hash": definition.definition_hash(),
        "project_id": project_id,
    }))
}

fn declarative_submission_data(
    existing_data: &Value,
    project_id: &str,
    prompt_ref: &str,
    definition: &DeclarativeWorkflowDefinition,
    ctx: &DeclarativeSubmissionRuntimeContext<'_>,
) -> Value {
    let mut data = existing_data.clone();
    if !data.is_object() {
        data = json!({});
    }
    let object = data
        .as_object_mut()
        .expect("declarative submission data was normalized to an object");
    object.insert(
        "definition_hash".to_string(),
        json!(definition.definition_hash()),
    );
    object.insert("project_id".to_string(), json!(project_id));
    object.insert(
        "submission_id".to_string(),
        json!(super::submission_id_for_data(existing_data, ctx.task_id)),
    );
    object.insert("task_id".to_string(), json!(ctx.task_id.as_str()));
    object.insert(
        "task_ids".to_string(),
        json!(task_id_history(existing_data, ctx.task_id)),
    );
    object.insert("prompt_ref".to_string(), json!(prompt_ref));
    object.insert(
        "prompt_chars".to_string(),
        json!(ctx.prompt.chars().count()),
    );
    object.insert("source".to_string(), json!(ctx.source));
    object.insert("external_id".to_string(), json!(ctx.external_id));
    object.insert(
        "execution_path".to_string(),
        json!(EXECUTION_PATH_WORKFLOW_RUNTIME),
    );
    crate::workflow_runtime_policy::merge_runtime_retry_policy(ctx.project_root, data)
}
