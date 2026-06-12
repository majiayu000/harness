use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_issue_submission_decision, build_prompt_submission_decision, DecisionValidator,
    IssueSubmissionDecisionInput, PromptSubmissionDecisionInput, ValidationContext,
    WorkflowDecision, WorkflowDecisionRecord, WorkflowDefinition, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject, PROMPT_TASK_DEFINITION_ID,
};
use serde_json::json;
use std::path::Path;

#[cfg(test)]
use crate::task_runner::{TaskStatus, TaskStore};
#[cfg(test)]
use harness_workflow::runtime::WorkflowCommandType;

const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
const EXECUTION_PATH_WORKFLOW_RUNTIME: &str = "workflow_runtime";
const PROMPT_TASK_DESCRIPTION: &str = "prompt task";

#[path = "workflow_runtime_submission/cancel.rs"]
mod cancel;
#[path = "workflow_runtime_submission/commit.rs"]
mod commit;
#[path = "workflow_runtime_submission/dependencies.rs"]
mod dependencies;
#[path = "workflow_runtime_submission/prompt_memory.rs"]
mod prompt_memory;
#[path = "workflow_runtime_submission/replay.rs"]
mod replay;

pub(crate) use cancel::{
    cancel_issue_submission_by_task_id, cancel_submission_by_workflow_id,
    RuntimeSubmissionCancelError, RuntimeSubmissionCancelOutcome,
};
use commit::{apply_decision, apply_prompt_decision};
pub(crate) use dependencies::{
    release_ready_issue_dependencies, release_ready_prompt_dependencies,
    resolve_issue_dependency_status, RuntimeDependencyStatus,
};
#[cfg(test)]
pub(crate) use prompt_memory::clear_prompt_submission_prompt_cache_for_test;
use prompt_memory::prompt_ref_for_submission;
#[cfg(test)]
use prompt_memory::{
    cache_prompt_submission_prompt, remove_prompt_submission_prompt,
    remove_terminal_prompt_submission_prompt,
};
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
}

pub(crate) struct IssueSubmissionRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub task_id: &'a TaskId,
    pub labels: &'a [String],
    pub force_execute: bool,
    pub additional_prompt: Option<&'a str>,
    pub depends_on: &'a [TaskId],
    pub dependencies_blocked: bool,
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowSubmissionRuntimeRecord {
    pub workflow_id: String,
    pub accepted: bool,
    pub decision_id: String,
    pub command_ids: Vec<String>,
    pub rejection_reason: Option<String>,
}

pub(crate) async fn record_issue_submission(
    store: &WorkflowRuntimeStore,
    ctx: IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    persist_issue_submission(store, &ctx).await
}

pub(crate) async fn record_prompt_submission(
    store: &WorkflowRuntimeStore,
    ctx: PromptSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    persist_prompt_submission(store, &ctx).await
}

pub(crate) async fn runtime_issue_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    store.get_instance_by_task_id(task_id.as_str()).await
}

pub(crate) fn runtime_issue_task_handle(instance: &WorkflowInstance) -> Option<TaskId> {
    runtime_submission_id(&instance.data).map(|submission_id| TaskId::from_str(&submission_id))
}

async fn persist_issue_submission(
    store: &WorkflowRuntimeStore,
    ctx: &IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, ctx.issue_number);
    upsert_github_issue_pr_definition(store).await?;
    let (instance, new_instance) = match store.get_instance(&workflow_id).await? {
        Some(instance) => (instance, false),
        None => (
            issue_instance(
                workflow_id,
                project_id.clone(),
                ctx.repo.map(ToOwned::to_owned),
                ctx.issue_number,
            ),
            true,
        ),
    };
    let submitted_data = issue_submission_data(ctx, &project_id, &instance.data);
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: ctx.task_id.as_str(),
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            labels: ctx.labels,
            force_execute: ctx.force_execute,
            additional_prompt: ctx.additional_prompt,
            depends_on: &depends_on_strings(ctx.depends_on),
            dependencies_blocked: ctx.dependencies_blocked,
        },
    );
    apply_decision(
        store,
        instance,
        new_instance,
        output.decision,
        ctx,
        submitted_data,
    )
    .await
}

async fn persist_prompt_submission(
    store: &WorkflowRuntimeStore,
    ctx: &PromptSubmissionRuntimeContext<'_>,
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
    let submitted_data =
        prompt_submission_data(ctx, &project_id, &instance.data, &prompt_ref, &depends_on);
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
        },
    );
    apply_prompt_decision(
        store,
        instance,
        new_instance,
        output.decision,
        ctx,
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
    mut instance: WorkflowInstance,
    decision: WorkflowDecision,
    event_id: String,
    accepted_data: Option<serde_json::Value>,
) -> anyhow::Result<WorkflowInstance> {
    let validation_context = if instance.is_terminal() {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    let validator = decision_validator_for_instance(&instance)?;
    if let Err(error) = validator.validate(&instance, &decision, &validation_context) {
        let reason = error.to_string();
        let record = WorkflowDecisionRecord::rejected(decision, Some(event_id), &reason);
        store.record_decision(&record).await?;
        anyhow::bail!(reason);
    }

    let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(event_id));
    store.record_decision(&record).await?;
    for command in &decision.commands {
        store
            .enqueue_command(&instance.id, Some(&record.id), command)
            .await?;
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(
        accepted_data.unwrap_or_else(|| instance.data.clone()),
        &decision.decision,
    );
    store.upsert_instance(&instance).await?;
    Ok(instance)
}

fn decision_validator_for_instance(
    instance: &WorkflowInstance,
) -> anyhow::Result<DecisionValidator> {
    match instance.definition_id.as_str() {
        GITHUB_ISSUE_PR_DEFINITION_ID => Ok(DecisionValidator::github_issue_pr()),
        PROMPT_TASK_DEFINITION_ID => Ok(DecisionValidator::prompt_task()),
        other => anyhow::bail!("workflow definition `{other}` cannot be committed by submission"),
    }
}

async fn upsert_github_issue_pr_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await
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

fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(json!({
        "project_id": project_id,
        "repo": repo,
        "issue_number": issue_number,
    }))
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
    .with_data(json!({
        "project_id": project_id,
    }))
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

fn issue_submission_data(
    ctx: &IssueSubmissionRuntimeContext<'_>,
    project_id: &str,
    existing_data: &serde_json::Value,
) -> serde_json::Value {
    crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
            "project_id": project_id,
            "repo": ctx.repo,
            "issue_number": ctx.issue_number,
            "submission_id": submission_id_for_data(existing_data, ctx.task_id),
            "task_id": ctx.task_id.as_str(),
            "task_ids": task_id_history(existing_data, ctx.task_id),
            "labels": ctx.labels,
            "force_execute": ctx.force_execute,
            "additional_prompt": ctx.additional_prompt,
            "depends_on": depends_on_strings(ctx.depends_on),
            "dependencies_blocked": ctx.dependencies_blocked,
            "source": ctx.source,
            "external_id": ctx.external_id,
        }),
    )
}

fn prompt_submission_data(
    ctx: &PromptSubmissionRuntimeContext<'_>,
    project_id: &str,
    existing_data: &serde_json::Value,
    prompt_ref: &str,
    depends_on: &[TaskId],
) -> serde_json::Value {
    crate::workflow_runtime_policy::merge_runtime_retry_policy(
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
        }),
    )
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
struct IssueSubmissionFields {
    task_id: String,
    repo: Option<String>,
    issue_number: u64,
    labels: Vec<String>,
    force_execute: bool,
    additional_prompt: Option<String>,
}

fn issue_submission_fields(instance: &WorkflowInstance) -> anyhow::Result<IssueSubmissionFields> {
    Ok(IssueSubmissionFields {
        task_id: string_field(&instance.data, "task_id")?,
        repo: optional_string_field(&instance.data, "repo"),
        issue_number: instance
            .data
            .get("issue_number")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing issue_number"))?,
        labels: string_array_field(&instance.data, "labels")?,
        force_execute: instance
            .data
            .get("force_execute")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        additional_prompt: optional_string_field(&instance.data, "additional_prompt"),
    })
}

#[derive(Debug)]
struct PromptSubmissionFields {
    task_id: String,
    prompt_ref: String,
    source: Option<String>,
    external_id: Option<String>,
}

fn prompt_submission_fields(instance: &WorkflowInstance) -> anyhow::Result<PromptSubmissionFields> {
    Ok(PromptSubmissionFields {
        task_id: string_field(&instance.data, "task_id")?,
        prompt_ref: string_field(&instance.data, "prompt_ref")?,
        source: optional_string_field(&instance.data, "source"),
        external_id: optional_string_field(&instance.data, "external_id"),
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

fn set_data_bool(mut data: serde_json::Value, key: &str, value: bool) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert(key.to_string(), json!(value));
    }
    data
}

fn set_data_string(mut data: serde_json::Value, key: &str, value: &str) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert(key.to_string(), json!(value));
    }
    data
}

#[cfg(test)]
#[path = "workflow_runtime_submission/atomicity_tests.rs"]
mod atomicity_tests;

#[cfg(test)]
#[path = "workflow_runtime_submission/identity_tests.rs"]
mod identity_tests;

#[cfg(test)]
#[path = "workflow_runtime_submission/dependency_tests.rs"]
mod dependency_tests;

#[cfg(test)]
#[path = "workflow_runtime_submission/replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
#[path = "workflow_runtime_submission_tests.rs"]
mod tests;
