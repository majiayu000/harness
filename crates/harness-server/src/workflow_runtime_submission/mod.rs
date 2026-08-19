use harness_core::config::isolation::IsolationTrustClass;
use harness_workflow::runtime::{
    build_issue_submission_decision, build_prompt_submission_decision,
    candidate_fanout_from_policy, candidate_fanout_from_value, continuation_value,
    prompt_continuation_state_from_data, CandidateFanoutRequest, DataProvenance, DecisionValidator,
    IssueSubmissionDecisionInput, PromptContinuationPolicy, PromptSubmissionDecisionInput,
    SubmissionMode, ValidationContext, WorkflowCommandStatus, WorkflowDecision,
    WorkflowDecisionTransition, WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore,
    WorkflowSubject, PROMPT_TASK_DEFINITION_ID,
};
use serde_json::json;
use std::path::Path;

const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
const EXECUTION_PATH_WORKFLOW_RUNTIME: &str = "workflow_runtime";
const PROMPT_TASK_DESCRIPTION: &str = "prompt task";
const GITHUB_TRACKER_SOURCE: &str = "github";

mod cancel;
mod commit;
mod declarative;
mod dependencies;
mod issue_submission;
mod prompt_memory;
mod replay;
pub(crate) mod runtime_models;
pub(crate) mod runtime_request;
pub(crate) mod runtime_state;

pub use runtime_models::TaskId;
pub(crate) use runtime_request::{
    fill_missing_repo_from_project, CreateTaskRequest, MAX_TASK_PRIORITY,
};

#[cfg(test)]
pub(crate) use cancel::cancel_issue_submission_by_task_id;
pub(crate) use cancel::{
    cancel_submission_by_workflow_id, RuntimeSubmissionCancelError, RuntimeSubmissionCancelOutcome,
};
use commit::{apply_decision, apply_prompt_decision, decision_validator_for_instance};
pub(crate) use declarative::{
    record_declarative_submission, resolve_declarative_definition_for_project,
    DeclarativeSubmissionRuntimeContext,
};
pub(crate) use dependencies::{
    release_ready_issue_dependencies, release_ready_prompt_dependencies,
    resolve_issue_dependency_status, RuntimeDependencyStatus,
};
use issue_submission::{
    insert_author_trust_class, issue_submission_fields, issue_tracker_external_id,
    issue_tracker_source,
};
#[cfg(test)]
use issue_submission::{issue_instance, issue_submission_data};
pub(crate) use issue_submission::{
    record_issue_submission, record_issue_submission_with_admission, IssueSubmissionRuntimeContext,
};
#[cfg(test)]
pub(crate) use prompt_memory::clear_prompt_submission_prompt_cache_for_test;
use prompt_memory::prompt_ref_for_submission;
#[cfg(test)]
use prompt_memory::{cache_prompt_submission_prompt, remove_terminal_prompt_submission_prompt};
pub(crate) use prompt_memory::{
    lookup_prompt_submission_prompt, lookup_prompt_submission_prompt_durable,
    remove_terminal_prompt_submission_payload,
};

pub(crate) struct PromptSubmissionRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub task_id: &'a TaskId,
    pub prompt: &'a str,
    pub depends_on: &'a [TaskId],
    pub serialization_depends_on: &'a [TaskId],
    pub dependencies_blocked: bool,
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub continuation: Option<&'a PromptContinuationPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowSubmissionRuntimeRecord {
    pub workflow_id: String,
    pub accepted: bool,
    pub decision_id: String,
    pub command_ids: Vec<String>,
    pub rejection_reason: Option<String>,
}

pub(crate) async fn record_prompt_submission(
    store: &WorkflowRuntimeStore,
    ctx: PromptSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    record_prompt_submission_with_policy(
        store,
        ctx,
        runtime_models::PromptExecutionPolicy::default(),
    )
    .await
}

pub(crate) async fn record_prompt_submission_with_policy(
    store: &WorkflowRuntimeStore,
    ctx: PromptSubmissionRuntimeContext<'_>,
    execution_policy: runtime_models::PromptExecutionPolicy,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    persist_prompt_submission(store, &ctx, &execution_policy).await
}

pub(crate) fn prompt_execution_policy(
    data: &serde_json::Value,
) -> anyhow::Result<Option<runtime_models::PromptExecutionPolicy>> {
    let Some(value) = data.get("execution_policy") else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| anyhow::anyhow!("runtime prompt has invalid execution_policy: {error}"))
}

pub(crate) async fn runtime_issue_by_submission_id(
    store: &WorkflowRuntimeStore,
    submission_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    store
        .get_instance_by_submission_id(submission_id.as_str())
        .await
}

pub(crate) fn runtime_issue_task_handle(instance: &WorkflowInstance) -> Option<TaskId> {
    crate::runtime_projection::runtime_submission_handle(&instance.data)
}

async fn persist_prompt_submission(
    store: &WorkflowRuntimeStore,
    ctx: &PromptSubmissionRuntimeContext<'_>,
    execution_policy: &runtime_models::PromptExecutionPolicy,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id = prompt_workflow_id(&project_id, ctx.external_id, ctx.task_id);
    upsert_prompt_task_definition(store).await?;
    let (instance, new_instance) = match store.get_instance(&workflow_id).await? {
        Some(instance) => (instance, false),
        None => (
            prompt_instance(
                workflow_id,
                project_id.clone(),
                prompt_subject_key(ctx.external_id, ctx.task_id),
            ),
            true,
        ),
    };
    let prompt_ref =
        prompt_ref_for_submission(&project_id, ctx.external_id, ctx.task_id, ctx.prompt);
    let depends_on = prompt_submission_dependency_ids(ctx);
    let submitted_data = prompt_submission_data(
        ctx,
        execution_policy,
        &project_id,
        &instance.data,
        &prompt_ref,
        &depends_on,
    );
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: ctx.task_id.as_str(),
            prompt: ctx.prompt,
            prompt_ref: &prompt_ref,
            source: ctx.source,
            external_id: ctx.external_id,
            depends_on: &depends_on_strings(&depends_on),
            dependencies_blocked: ctx.dependencies_blocked,
            continuation: ctx.continuation,
        },
    )?;
    apply_prompt_decision(
        store,
        instance,
        new_instance,
        output.decision,
        ctx,
        execution_policy,
        submitted_data,
    )
    .await
}

fn prompt_submission_dependency_ids(ctx: &PromptSubmissionRuntimeContext<'_>) -> Vec<TaskId> {
    let mut depends_on =
        Vec::with_capacity(ctx.depends_on.len() + ctx.serialization_depends_on.len());
    depends_on.extend(ctx.depends_on.iter().cloned());
    for dep_id in ctx.serialization_depends_on {
        if !depends_on.iter().any(|existing| existing == dep_id) {
            depends_on.push(dep_id.clone());
        }
    }
    depends_on
}

async fn commit_runtime_decision(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    decision: WorkflowDecision,
    event_type: &'static str,
    source: &'static str,
    event_payload: serde_json::Value,
    accepted_data: Option<serde_json::Value>,
) -> anyhow::Result<WorkflowInstance> {
    let validator = decision_validator_for_instance(store, &instance)?;
    commit_runtime_decision_with_validator(
        store,
        instance,
        decision,
        event_type,
        source,
        event_payload,
        accepted_data,
        validator,
        false,
    )
    .await
}

async fn commit_runtime_decision_with_validator(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    decision: WorkflowDecision,
    event_type: &'static str,
    source: &'static str,
    event_payload: serde_json::Value,
    accepted_data: Option<serde_json::Value>,
    validator: DecisionValidator,
    allow_missing_pinned_cancel: bool,
) -> anyhow::Result<WorkflowInstance> {
    let mut validation_context = if instance.is_terminal_with_registry(store.definition_registry())
    {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    if allow_missing_pinned_cancel {
        validation_context = validation_context.allow_missing_pinned_cancel();
    }
    let expected_state = instance.state.clone();
    let mut final_instance = instance.clone();
    final_instance.state = decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);
    classify_submission_data(
        &mut final_instance,
        merge_last_decision(
            accepted_data.unwrap_or_else(|| instance.data.clone()),
            &decision.decision,
        ),
    )?;
    let record = store
        .apply_decision_transition_with_validator(
            WorkflowDecisionTransition {
                expected_state: &expected_state,
                create_if_missing: None,
                event_type,
                source,
                payload: event_payload,
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            &validator,
            validation_context,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "workflow state changed before runtime submission transition could be committed"
            )
        })?;
    if !record.accepted {
        anyhow::bail!(
            "{}",
            record
                .rejection_reason
                .unwrap_or_else(|| "decision rejected".to_string())
        );
    }
    Ok(final_instance)
}

async fn upsert_prompt_task_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "Prompt task workflow",
        ))
        .await
}

fn prompt_instance(
    workflow_id: String,
    project_id: String,
    subject_key: String,
) -> WorkflowInstance {
    WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "submitted",
        WorkflowSubject::new("prompt", subject_key),
    )
    .with_id(workflow_id)
    .with_classified_data(json!({ "project_id": project_id }), DataProvenance::Server)
}

/// Build an updated `workflow.data` document as a plain value.
///
/// These stage a document; they do not persist one. Classification happens at
/// the single commit point, where `classify_submission_data` assigns every
/// field its provenance before the transition is written. Keeping the staging
/// step provenance-free means there is exactly one place that decides what a
/// submission field's origin is.
pub(super) fn set_data_bool(
    mut data: serde_json::Value,
    key: &str,
    value: bool,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert(key.to_string(), json!(value));
    }
    data
}

/// See [`set_data_bool`].
pub(super) fn set_data_string(
    mut data: serde_json::Value,
    key: &str,
    value: &str,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert(key.to_string(), json!(value));
    }
    data
}

pub(super) fn classify_submission_data(
    instance: &mut WorkflowInstance,
    data: serde_json::Value,
) -> anyhow::Result<()> {
    instance.replace_data_with_field_provenance(data, submission_field_provenance)
}

pub(super) fn submission_field_provenance(field: &str) -> DataProvenance {
    match field {
        "additional_prompt"
        | "depends_on"
        | "continuation"
        | "external_id"
        | "labels"
        | "last_external_state"
        | "required_depends_on"
        | "serialization_depends_on"
        | "source"
        | "tracker_external_id" => DataProvenance::External,
        "review_summary" | "summary" => DataProvenance::Agent,
        _ => DataProvenance::Server,
    }
}

pub(crate) fn prompt_workflow_id(
    project_id: &str,
    external_id: Option<&str>,
    task_id: &TaskId,
) -> String {
    format!(
        "{project_id}::prompt:{}",
        prompt_subject_key(external_id, task_id)
    )
}

fn prompt_subject_key(external_id: Option<&str>, task_id: &TaskId) -> String {
    external_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| task_id.as_str())
        .to_string()
}

fn prompt_submission_data(
    ctx: &PromptSubmissionRuntimeContext<'_>,
    execution_policy: &runtime_models::PromptExecutionPolicy,
    project_id: &str,
    existing_data: &serde_json::Value,
    prompt_ref: &str,
    depends_on: &[TaskId],
) -> serde_json::Value {
    let mut data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
            "project_id": project_id,
            "submission_id": submission_id_for_data(existing_data, ctx.task_id),
            "task_id": ctx.task_id.as_str(),
            "task_ids": task_id_history(existing_data, ctx.task_id),
            "prompt_summary": PROMPT_TASK_DESCRIPTION,
            "prompt_chars": ctx.prompt.chars().count(),
            "prompt_ref": prompt_ref,
            "depends_on": depends_on_strings(depends_on),
            "required_depends_on": depends_on_strings(ctx.depends_on),
            "serialization_depends_on": depends_on_strings(ctx.serialization_depends_on),
            "dependencies_blocked": ctx.dependencies_blocked,
            "source": ctx.source,
            "external_id": ctx.external_id,
            "execution_policy": execution_policy,
        }),
    );
    if let (Some(object), Some(policy)) = (data.as_object_mut(), ctx.continuation) {
        object.insert("continuation".to_string(), continuation_value(policy));
    }
    data
}

fn merge_last_decision(mut data: serde_json::Value, decision: &str) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert(
            "execution_path".to_string(),
            json!(EXECUTION_PATH_WORKFLOW_RUNTIME),
        );
    }
    data
}

#[derive(Debug)]
struct PromptSubmissionFields {
    task_id: String,
    prompt_ref: String,
    source: Option<String>,
    external_id: Option<String>,
    continuation: Option<PromptContinuationPolicy>,
}

fn prompt_submission_fields(instance: &WorkflowInstance) -> anyhow::Result<PromptSubmissionFields> {
    let continuation = prompt_continuation_state_from_data(&instance.data)
        .map_err(anyhow::Error::msg)?
        .map(|state| state.policy);
    Ok(PromptSubmissionFields {
        task_id: string_field(&instance.data, "task_id")?,
        prompt_ref: string_field(&instance.data, "prompt_ref")?,
        source: optional_string_field(&instance.data, "source"),
        external_id: optional_string_field(&instance.data, "external_id"),
        continuation,
    })
}

fn task_ids_from_data(data: &serde_json::Value, field: &str) -> anyhow::Result<Vec<TaskId>> {
    Ok(string_array_field(data, field)?
        .into_iter()
        .map(|task_id| TaskId::from_str(&task_id))
        .collect())
}

fn task_id_history(existing_data: &serde_json::Value, new_task_id: &TaskId) -> Vec<String> {
    let mut task_ids = Vec::new();
    if let Some(submission_id) = optional_string_field(existing_data, "submission_id") {
        push_unique_task_id(&mut task_ids, submission_id);
    }
    if let Ok(existing_ids) = string_array_field(existing_data, "task_ids") {
        for task_id in existing_ids {
            push_unique_task_id(&mut task_ids, task_id);
        }
    }
    if let Some(task_id) = optional_string_field(existing_data, "task_id") {
        push_unique_task_id(&mut task_ids, task_id);
    }
    push_unique_task_id(&mut task_ids, new_task_id.as_str().to_string());
    task_ids
}

fn submission_id_for_data(existing_data: &serde_json::Value, new_task_id: &TaskId) -> String {
    runtime_submission_id(existing_data).unwrap_or_else(|| new_task_id.as_str().to_string())
}

fn runtime_submission_id(data: &serde_json::Value) -> Option<String> {
    optional_string_field(data, "submission_id")
        .or_else(|| {
            string_array_field(data, "task_ids")
                .ok()
                .and_then(|task_ids| task_ids.into_iter().next())
        })
        .or_else(|| optional_string_field(data, "task_id"))
}

fn push_unique_task_id(task_ids: &mut Vec<String>, task_id: String) {
    if !task_ids.iter().any(|existing| existing == &task_id) {
        task_ids.push(task_id);
    }
}

fn depends_on_strings(depends_on: &[TaskId]) -> Vec<String> {
    depends_on
        .iter()
        .map(|task_id| task_id.as_str().to_string())
        .collect()
}

fn string_field(data: &serde_json::Value, field: &str) -> anyhow::Result<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing {field}"))
}

fn optional_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

fn string_array_field(data: &serde_json::Value, field: &str) -> anyhow::Result<Vec<String>> {
    let Some(value) = data.get(field) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        anyhow::bail!("runtime issue workflow field {field} must be an array");
    };
    items
        .iter()
        .map(|item| {
            item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                anyhow::anyhow!("runtime issue workflow field {field} must contain strings")
            })
        })
        .collect()
}

#[cfg(test)]
mod atomicity_tests;
#[cfg(test)]
mod continuation_tests;
#[cfg(test)]
mod declarative_cancel_tests;
#[cfg(test)]
mod declarative_project_tests;
#[cfg(test)]
mod declarative_tests;
#[cfg(test)]
mod dependency_tests;
#[cfg(test)]
mod identity_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
#[path = "../workflow_runtime_submission_tests.rs"]
mod tests;
#[cfg(test)]
mod trust_tests;
