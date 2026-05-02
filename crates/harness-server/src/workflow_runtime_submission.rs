use crate::task_runner::{TaskId, TaskStatus, TaskStore};
use harness_workflow::runtime::{
    build_issue_submission_decision, build_prompt_submission_decision, DecisionValidator,
    IssueSubmissionDecisionInput, PromptSubmissionDecisionInput, ValidationContext,
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    PROMPT_TASK_DEFINITION_ID,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
};

const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
const EXECUTION_PATH_WORKFLOW_RUNTIME: &str = "workflow_runtime";
const PROMPT_TASK_DESCRIPTION: &str = "prompt task";

static PROMPT_SUBMISSION_PROMPTS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptDependencyReleaseOutcome {
    Released,
    MissingPrompt,
}

pub(crate) fn lookup_prompt_submission_prompt(prompt_ref: &str) -> Option<String> {
    prompt_submission_prompts()
        .lock()
        .ok()
        .and_then(|prompts| prompts.get(prompt_ref).cloned())
}

fn cache_prompt_submission_prompt(prompt_ref: &str, prompt: &str) {
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.insert(prompt_ref.to_string(), prompt.to_string());
    }
}

fn remove_prompt_submission_prompt(prompt_ref: Option<&str>) {
    let Some(prompt_ref) = prompt_ref else {
        return;
    };
    if let Ok(mut prompts) = prompt_submission_prompts().lock() {
        prompts.remove(prompt_ref);
    }
}

pub(crate) fn remove_terminal_prompt_submission_prompt(instance: &WorkflowInstance) {
    if instance.definition_id != PROMPT_TASK_DEFINITION_ID || !instance.is_terminal() {
        return;
    }
    remove_prompt_submission_prompt(optional_string_field(&instance.data, "prompt_ref").as_deref());
}

fn prompt_submission_prompts() -> &'static Mutex<HashMap<String, String>> {
    PROMPT_SUBMISSION_PROMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prompt_ref_for_submission(
    project_id: &str,
    external_id: Option<&str>,
    task_id: &TaskId,
    prompt: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(external_id.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(task_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("prompt-memory:{digest_hex}")
}

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DependencyReleaseSummary {
    pub released: usize,
    pub failed: usize,
    pub waiting: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeDependencyStatus {
    Done,
    Failed,
    Cancelled,
    Waiting,
}

pub(crate) async fn resolve_issue_dependency_status(
    store: Option<&WorkflowRuntimeStore>,
    tasks: &TaskStore,
    task_id: &TaskId,
) -> anyhow::Result<RuntimeDependencyStatus> {
    match tasks.dep_status(task_id).await {
        Some(TaskStatus::Done) => return Ok(RuntimeDependencyStatus::Done),
        Some(TaskStatus::Failed) => return Ok(RuntimeDependencyStatus::Failed),
        Some(TaskStatus::Cancelled) => return Ok(RuntimeDependencyStatus::Cancelled),
        Some(_) => return Ok(RuntimeDependencyStatus::Waiting),
        None => {}
    }

    let Some(store) = store else {
        return Ok(RuntimeDependencyStatus::Waiting);
    };
    let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(RuntimeDependencyStatus::Waiting);
    };
    Ok(match instance.state.as_str() {
        "done" => RuntimeDependencyStatus::Done,
        "failed" => RuntimeDependencyStatus::Failed,
        "cancelled" => RuntimeDependencyStatus::Cancelled,
        _ => RuntimeDependencyStatus::Waiting,
    })
}

pub(crate) async fn release_ready_issue_dependencies(
    store: &WorkflowRuntimeStore,
    tasks: &TaskStore,
    limit: i64,
) -> anyhow::Result<DependencyReleaseSummary> {
    let instances = store
        .list_instances_by_state(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_dependencies",
            limit,
        )
        .await?;
    let mut summary = DependencyReleaseSummary::default();
    for instance in instances {
        let depends_on = match task_ids_from_data(&instance.data, "depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed issue data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let mut all_done = true;
        let mut terminal_failure: Option<(TaskId, &'static str)> = None;
        for dep_id in &depends_on {
            match resolve_issue_dependency_status(Some(store), tasks, dep_id).await? {
                RuntimeDependencyStatus::Done => {}
                RuntimeDependencyStatus::Failed => {
                    terminal_failure = Some((dep_id.clone(), "failed"));
                    break;
                }
                RuntimeDependencyStatus::Cancelled => {
                    terminal_failure = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                RuntimeDependencyStatus::Waiting => all_done = false,
            }
        }
        if let Some((dep_id, label)) = terminal_failure {
            fail_issue_for_dependency(store, instance, &dep_id, label).await?;
            summary.failed += 1;
        } else if all_done {
            release_issue_after_dependencies(store, instance, &depends_on).await?;
            summary.released += 1;
        } else {
            store.touch_instance(&instance.id).await?;
            summary.waiting += 1;
        }
    }
    Ok(summary)
}

pub(crate) async fn release_ready_prompt_dependencies(
    store: &WorkflowRuntimeStore,
    tasks: &TaskStore,
    limit: i64,
) -> anyhow::Result<DependencyReleaseSummary> {
    let instances = store
        .list_instances_by_state(PROMPT_TASK_DEFINITION_ID, "awaiting_dependencies", limit)
        .await?;
    let mut summary = DependencyReleaseSummary::default();
    for instance in instances {
        let depends_on = match task_ids_from_data(&instance.data, "depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let serialization_depends_on = match task_ids_from_data(
            &instance.data,
            "serialization_depends_on",
        ) {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt serialization data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let required_depends_on = match task_ids_from_data(&instance.data, "required_depends_on") {
            Ok(depends_on) => depends_on,
            Err(error) => {
                tracing::warn!(
                    workflow_id = %instance.id,
                    "workflow runtime dependency release skipped malformed prompt required dependency data: {error}"
                );
                summary.skipped += 1;
                store.touch_instance(&instance.id).await?;
                continue;
            }
        };
        let mut all_done = true;
        let mut terminal_failure: Option<(TaskId, &'static str)> = None;
        for dep_id in &depends_on {
            match resolve_issue_dependency_status(Some(store), tasks, dep_id).await? {
                RuntimeDependencyStatus::Done => {}
                RuntimeDependencyStatus::Failed
                    if serialization_depends_on.iter().any(|id| id == dep_id)
                        && !required_depends_on.iter().any(|id| id == dep_id) => {}
                RuntimeDependencyStatus::Failed => {
                    terminal_failure = Some((dep_id.clone(), "failed"));
                    break;
                }
                RuntimeDependencyStatus::Cancelled
                    if serialization_depends_on.iter().any(|id| id == dep_id)
                        && !required_depends_on.iter().any(|id| id == dep_id) => {}
                RuntimeDependencyStatus::Cancelled => {
                    terminal_failure = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                RuntimeDependencyStatus::Waiting => all_done = false,
            }
        }
        if let Some((dep_id, label)) = terminal_failure {
            fail_prompt_for_dependency(store, instance, &dep_id, label).await?;
            summary.failed += 1;
        } else if all_done {
            match release_prompt_after_dependencies(store, instance, &depends_on).await? {
                PromptDependencyReleaseOutcome::Released => summary.released += 1,
                PromptDependencyReleaseOutcome::MissingPrompt => summary.failed += 1,
            }
        } else {
            store.touch_instance(&instance.id).await?;
            summary.waiting += 1;
        }
    }
    Ok(summary)
}

pub(crate) async fn runtime_issue_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    store.get_instance_by_task_id(task_id.as_str()).await
}

pub(crate) fn runtime_issue_task_handle(instance: &WorkflowInstance) -> Option<TaskId> {
    string_array_field(&instance.data, "task_ids")
        .ok()
        .and_then(|task_ids| task_ids.into_iter().next())
        .or_else(|| optional_string_field(&instance.data, "task_id"))
        .map(|task_id| TaskId::from_str(&task_id))
}

pub(crate) async fn cancel_issue_submission_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &TaskId,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(None);
    };
    if instance.is_terminal() {
        return Ok(Some(instance));
    }
    let is_prompt = instance.definition_id == PROMPT_TASK_DEFINITION_ID;
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
                "task_id": task_id.as_str(),
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
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
        format!("{command_prefix}:{}:cancel", task_id.as_str()),
        json!({ "task_id": task_id.as_str() }),
    ))
    .high_confidence();
    let mut cancelled = commit_runtime_decision(store, instance, decision, event.id, None).await?;
    let commands = store.commands_for(&cancelled.id).await?;
    for command in commands {
        if matches!(command.status.as_str(), "pending" | "dispatched") {
            store
                .cancel_command_and_unfinished_runtime_jobs(
                    &command.id,
                    "cancel_issue_submission",
                    "Runtime issue submission was cancelled before execution.",
                )
                .await?;
        }
    }
    cancelled.data = set_data_bool(cancelled.data, "cancelled", true);
    store.upsert_instance(&cancelled).await?;
    if is_prompt {
        remove_prompt_submission_prompt(
            optional_string_field(&cancelled.data, "prompt_ref").as_deref(),
        );
    }
    Ok(Some(cancelled))
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

async fn apply_decision(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    new_instance: bool,
    decision: WorkflowDecision,
    ctx: &IssueSubmissionRuntimeContext<'_>,
    accepted_data: serde_json::Value,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let validation_context = if instance.is_terminal() {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    let validation =
        DecisionValidator::github_issue_pr().validate(&instance, &decision, &validation_context);
    if new_instance {
        store.upsert_instance(&instance).await?;
    }
    let event = store
        .append_event(
            &instance.id,
            "IssueSubmitted",
            "workflow_runtime_submission",
            json!({
                "task_id": ctx.task_id.as_str(),
                "repo": ctx.repo,
                "issue_number": ctx.issue_number,
                "labels": ctx.labels,
                "force_execute": ctx.force_execute,
                "additional_prompt": ctx.additional_prompt,
                "depends_on": depends_on_strings(ctx.depends_on),
                "dependencies_blocked": ctx.dependencies_blocked,
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id)),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(decision, Some(event.id), &reason);
            store.record_decision(&record).await?;
            return Ok(WorkflowSubmissionRuntimeRecord {
                workflow_id: instance.id,
                accepted: false,
                decision_id: record.id,
                command_ids: Vec::new(),
                rejection_reason: Some(reason),
            });
        }
    };
    store.record_decision(&record).await?;
    let mut command_ids = Vec::with_capacity(decision.commands.len());
    for command in &decision.commands {
        command_ids.push(
            store
                .enqueue_command(&instance.id, Some(&record.id), command)
                .await?,
        );
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(accepted_data, &decision.decision);
    store.upsert_instance(&instance).await?;
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: instance.id,
        accepted: true,
        decision_id: record.id,
        command_ids,
        rejection_reason: None,
    })
}

async fn apply_prompt_decision(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    new_instance: bool,
    decision: WorkflowDecision,
    ctx: &PromptSubmissionRuntimeContext<'_>,
    accepted_data: serde_json::Value,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let validation_context = if instance.is_terminal() {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    let validation =
        DecisionValidator::prompt_task().validate(&instance, &decision, &validation_context);
    if new_instance {
        store.upsert_instance(&instance).await?;
    }
    let event = store
        .append_event(
            &instance.id,
            "PromptSubmitted",
            "workflow_runtime_submission",
            json!({
                "task_id": ctx.task_id.as_str(),
                "prompt_chars": ctx.prompt.chars().count(),
                "depends_on": depends_on_strings(ctx.depends_on),
                "serialization_depends_on": depends_on_strings(ctx.serialization_depends_on),
                "dependencies_blocked": ctx.dependencies_blocked,
                "source": ctx.source,
                "external_id": ctx.external_id,
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id)),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(decision, Some(event.id), &reason);
            store.record_decision(&record).await?;
            return Ok(WorkflowSubmissionRuntimeRecord {
                workflow_id: instance.id,
                accepted: false,
                decision_id: record.id,
                command_ids: Vec::new(),
                rejection_reason: Some(reason),
            });
        }
    };
    store.record_decision(&record).await?;
    let prompt_ref = string_field(&accepted_data, "prompt_ref")?;
    let previous_prompt_ref = optional_string_field(&instance.data, "prompt_ref");
    if previous_prompt_ref.as_deref() != Some(prompt_ref.as_str()) {
        remove_prompt_submission_prompt(previous_prompt_ref.as_deref());
    }
    cache_prompt_submission_prompt(&prompt_ref, ctx.prompt);
    let mut command_ids = Vec::with_capacity(decision.commands.len());
    for command in &decision.commands {
        command_ids.push(
            store
                .enqueue_command(&instance.id, Some(&record.id), command)
                .await?,
        );
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(accepted_data, &decision.decision);
    store.upsert_instance(&instance).await?;
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: instance.id,
        accepted: true,
        decision_id: record.id,
        command_ids,
        rejection_reason: None,
    })
}

async fn release_issue_after_dependencies(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    depends_on: &[TaskId],
) -> anyhow::Result<()> {
    let fields = issue_submission_fields(&instance)?;
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: &fields.task_id,
            repo: fields.repo.as_deref(),
            issue_number: fields.issue_number,
            labels: &fields.labels,
            force_execute: fields.force_execute,
            additional_prompt: fields.additional_prompt.as_deref(),
            depends_on: &depends_on_strings(depends_on),
            dependencies_blocked: false,
        },
    );
    let event = store
        .append_event(
            &instance.id,
            "IssueDependenciesSatisfied",
            "workflow_runtime_submission",
            json!({
                "task_id": fields.task_id,
                "repo": fields.repo,
                "issue_number": fields.issue_number,
                "depends_on": depends_on_strings(depends_on),
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let data = set_data_bool(instance.data.clone(), "dependencies_blocked", false);
    commit_runtime_decision(store, instance, output.decision, event.id, Some(data)).await?;
    Ok(())
}

async fn fail_issue_for_dependency(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    dependency_id: &TaskId,
    dependency_status: &str,
) -> anyhow::Result<()> {
    let event = store
        .append_event(
            &instance.id,
            "IssueDependencyFailed",
            "workflow_runtime_submission",
            json!({
                "dependency_task_id": dependency_id.as_str(),
                "dependency_status": dependency_status,
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "dependency_failed",
        "failed",
        format!(
            "dependency task {} {} before runtime issue submission could start",
            dependency_id.as_str(),
            dependency_status
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("issue-submit:{}:dependency-failed", dependency_id.as_str()),
        json!({
            "dependency_task_id": dependency_id.as_str(),
            "dependency_status": dependency_status,
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        set_data_string(
            instance.data.clone(),
            "dependency_failure_task_id",
            dependency_id.as_str(),
        ),
        "dependency_failure_status",
        dependency_status,
    );
    commit_runtime_decision(store, instance, decision, event.id, Some(data)).await?;
    Ok(())
}

async fn release_prompt_after_dependencies(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    depends_on: &[TaskId],
) -> anyhow::Result<PromptDependencyReleaseOutcome> {
    let fields = prompt_submission_fields(&instance)?;
    let Some(prompt) = lookup_prompt_submission_prompt(&fields.prompt_ref) else {
        fail_prompt_for_missing_prompt(store, instance, &fields).await?;
        return Ok(PromptDependencyReleaseOutcome::MissingPrompt);
    };
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: &fields.task_id,
            prompt: &prompt,
            prompt_ref: &fields.prompt_ref,
            source: fields.source.as_deref(),
            external_id: fields.external_id.as_deref(),
            depends_on: &depends_on_strings(depends_on),
            dependencies_blocked: false,
        },
    );
    let event = store
        .append_event(
            &instance.id,
            "PromptDependenciesSatisfied",
            "workflow_runtime_submission",
            json!({
                "task_id": fields.task_id,
                "depends_on": depends_on_strings(depends_on),
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let data = set_data_bool(instance.data.clone(), "dependencies_blocked", false);
    commit_runtime_decision(store, instance, output.decision, event.id, Some(data)).await?;
    Ok(PromptDependencyReleaseOutcome::Released)
}

async fn fail_prompt_for_dependency(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    dependency_id: &TaskId,
    dependency_status: &str,
) -> anyhow::Result<()> {
    let prompt_ref = optional_string_field(&instance.data, "prompt_ref");
    let event = store
        .append_event(
            &instance.id,
            "PromptDependencyFailed",
            "workflow_runtime_submission",
            json!({
                "dependency_task_id": dependency_id.as_str(),
                "dependency_status": dependency_status,
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "dependency_failed",
        "failed",
        format!(
            "dependency task {} {} before runtime prompt submission could start",
            dependency_id.as_str(),
            dependency_status
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("prompt-submit:{}:dependency-failed", dependency_id.as_str()),
        json!({
            "dependency_task_id": dependency_id.as_str(),
            "dependency_status": dependency_status,
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        set_data_string(
            instance.data.clone(),
            "dependency_failure_task_id",
            dependency_id.as_str(),
        ),
        "dependency_failure_status",
        dependency_status,
    );
    commit_runtime_decision(store, instance, decision, event.id, Some(data)).await?;
    remove_prompt_submission_prompt(prompt_ref.as_deref());
    Ok(())
}

async fn fail_prompt_for_missing_prompt(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    fields: &PromptSubmissionFields,
) -> anyhow::Result<()> {
    let event = store
        .append_event(
            &instance.id,
            "PromptSubmissionPromptMissing",
            "workflow_runtime_submission",
            json!({
                "task_id": fields.task_id.as_str(),
                "prompt_ref": fields.prompt_ref.as_str(),
                "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
            }),
        )
        .await?;
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "prompt_unavailable",
        "failed",
        "prompt-only runtime submission cannot resume because its in-memory prompt is unavailable",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        format!("prompt-submit:{}:prompt-missing", fields.task_id),
        json!({
            "task_id": fields.task_id.as_str(),
            "prompt_ref": fields.prompt_ref.as_str(),
        }),
    ))
    .high_confidence();
    let data = set_data_string(
        instance.data.clone(),
        "failure_reason",
        "prompt unavailable",
    );
    commit_runtime_decision(store, instance, decision, event.id, Some(data)).await?;
    Ok(())
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
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;

    async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
        let database_url = resolve_database_url(None)?;
        WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
    }

    #[tokio::test]
    async fn issue_submission_records_pending_runtime_implementation_command() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("task-1");
        let labels = vec!["bug".to_string(), "force-execute".to_string()];

        let result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 42,
                task_id: &task_id,
                labels: &labels,
                force_execute: true,
                additional_prompt: Some("include the regression test first"),
                depends_on: &[],
                dependencies_blocked: false,
                source: Some("github"),
                external_id: Some("issue:42"),
            },
        )
        .await?;

        assert!(result.accepted);
        assert_eq!(result.command_ids.len(), 1);
        assert!(result.rejection_reason.is_none());

        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            42,
        );
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow should be persisted");
        assert_eq!(instance.state, "implementing");
        assert_eq!(instance.data["task_id"], "task-1");
        assert_eq!(instance.data["task_ids"], serde_json::json!(["task-1"]));
        assert_eq!(
            instance.data["additional_prompt"],
            "include the regression test first"
        );
        assert_eq!(instance.data["source"], "github");
        assert_eq!(instance.data["external_id"], "issue:42");
        assert_eq!(instance.data["last_decision"], "submit_issue");
        assert_eq!(
            instance.data["execution_path"],
            EXECUTION_PATH_WORKFLOW_RUNTIME
        );

        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "IssueSubmitted"));

        let commands = store.commands_for(&workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
        assert_eq!(
            commands[0].command.command["additional_prompt"],
            "include the regression test first"
        );
        assert_eq!(store.pending_commands(10).await?.len(), 1);
        assert!(store
            .runtime_jobs_for_command(&commands[0].id)
            .await?
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_submission_records_pending_runtime_implementation_command() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("prompt-task-1");

        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "fix the prompt-only issue",
                depends_on: &[],
                serialization_depends_on: &[],
                dependencies_blocked: false,
                source: Some("dashboard"),
                external_id: Some("manual:prompt:1"),
            },
        )
        .await?;

        assert!(result.accepted);
        assert_eq!(result.command_ids.len(), 1);
        assert!(result.rejection_reason.is_none());

        let workflow_id = prompt_workflow_id(
            &project_root.to_string_lossy(),
            Some("manual:prompt:1"),
            &task_id,
        );
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow should be persisted");
        assert_eq!(instance.definition_id, PROMPT_TASK_DEFINITION_ID);
        assert_eq!(instance.state, "implementing");
        assert_eq!(instance.data["task_id"], "prompt-task-1");
        assert_eq!(
            instance.data["task_ids"],
            serde_json::json!(["prompt-task-1"])
        );
        assert!(instance.data.get("prompt").is_none());
        assert_eq!(instance.data["prompt_summary"], PROMPT_TASK_DESCRIPTION);
        assert_eq!(
            instance.data["prompt_chars"],
            "fix the prompt-only issue".chars().count()
        );
        let prompt_ref = instance.data["prompt_ref"]
            .as_str()
            .expect("prompt ref should be persisted");
        assert_eq!(
            lookup_prompt_submission_prompt(prompt_ref).as_deref(),
            Some("fix the prompt-only issue")
        );
        assert_eq!(instance.data["source"], "dashboard");
        assert_eq!(instance.data["external_id"], "manual:prompt:1");
        assert_eq!(instance.data["last_decision"], "submit_prompt");
        assert_eq!(
            instance.data["execution_path"],
            EXECUTION_PATH_WORKFLOW_RUNTIME
        );

        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "PromptSubmitted"));

        let commands = store.commands_for(&workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(
            commands[0].command.activity_name(),
            Some("implement_prompt")
        );
        assert!(commands[0].command.command.get("prompt").is_none());
        assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
        assert_eq!(store.pending_commands(10).await?.len(), 1);
        assert!(store
            .runtime_jobs_for_command(&commands[0].id)
            .await?
            .is_empty());
        Ok(())
    }

    #[test]
    fn terminal_prompt_submission_removes_cached_prompt() {
        let prompt_ref = "prompt-memory:test-cleanup";
        cache_prompt_submission_prompt(prompt_ref, "sensitive prompt body");
        let instance = WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "done",
            WorkflowSubject::new("prompt", "manual:prompt:cleanup"),
        )
        .with_data(json!({
            "task_id": "runtime-prompt-cleanup",
            "prompt_ref": prompt_ref,
        }));

        remove_terminal_prompt_submission_prompt(&instance);

        assert_eq!(lookup_prompt_submission_prompt(prompt_ref), None);
    }

    #[tokio::test]
    async fn prompt_resubmission_removes_previous_cached_prompt() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy().into_owned();
        let task_id = TaskId::from_str("runtime-prompt-cache-retry");
        let external_id = "manual:prompt:cache-retry";
        let old_prompt_ref = "prompt-memory:test-resubmit-old";
        cache_prompt_submission_prompt(old_prompt_ref, "old sensitive prompt body");
        let workflow_id = prompt_workflow_id(&project_id, Some(external_id), &task_id);
        store
            .upsert_instance(
                &WorkflowInstance::new(
                    PROMPT_TASK_DEFINITION_ID,
                    1,
                    "blocked",
                    WorkflowSubject::new("prompt", external_id),
                )
                .with_id(workflow_id)
                .with_data(json!({
                    "project_id": project_id,
                    "task_id": "old-prompt-task",
                    "prompt_ref": old_prompt_ref,
                    "source": "dashboard",
                    "external_id": external_id,
                })),
            )
            .await?;

        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "new prompt body",
                depends_on: &[],
                serialization_depends_on: &[],
                dependencies_blocked: false,
                source: Some("dashboard"),
                external_id: Some(external_id),
            },
        )
        .await?;

        assert!(result.accepted);
        assert_eq!(lookup_prompt_submission_prompt(old_prompt_ref), None);
        let instance = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        let new_prompt_ref = instance.data["prompt_ref"]
            .as_str()
            .expect("new prompt ref should be persisted");
        assert_ne!(new_prompt_ref, old_prompt_ref);
        assert_eq!(
            lookup_prompt_submission_prompt(new_prompt_ref).as_deref(),
            Some("new prompt body")
        );
        Ok(())
    }

    #[tokio::test]
    async fn issue_submission_preserves_prior_task_handles_for_lookup() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let labels = vec!["bug".to_string()];
        let first_task_id = TaskId::from_str("runtime-handle-first");
        let second_task_id = TaskId::from_str("runtime-handle-second");

        record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 44,
                task_id: &first_task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;
        let result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 44,
                task_id: &second_task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;

        assert!(result.accepted);
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        assert_eq!(
            workflow.data["task_ids"],
            serde_json::json!(["runtime-handle-first", "runtime-handle-second"])
        );
        assert_eq!(
            runtime_issue_task_handle(&workflow)
                .expect("runtime workflow should expose a stable handle")
                .as_str(),
            "runtime-handle-first"
        );
        assert_eq!(
            runtime_issue_by_task_id(&store, &first_task_id)
                .await?
                .expect("first handle should resolve")
                .id,
            workflow.id
        );
        assert_eq!(
            runtime_issue_by_task_id(&store, &second_task_id)
                .await?
                .expect("second handle should resolve")
                .id,
            workflow.id
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancel_issue_submission_cancels_dispatched_runtime_jobs() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("runtime-handle-cancel");
        let labels = vec!["bug".to_string()];
        let result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 42,
                task_id: &task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;
        let command_id = result
            .command_ids
            .first()
            .expect("issue submission should enqueue an implementation command")
            .clone();
        store
            .enqueue_runtime_job_for_pending_command(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexExec,
                "default",
                json!({ "task_id": task_id.as_str() }),
                None,
            )
            .await?;
        assert_eq!(
            store
                .get_command(&command_id)
                .await?
                .expect("command should exist")
                .status,
            "dispatched"
        );

        let cancelled = cancel_issue_submission_by_task_id(&store, &task_id)
            .await?
            .expect("runtime issue submission should resolve by task id");
        assert_eq!(cancelled.state, "cancelled");

        let commands = store.commands_for(&result.workflow_id).await?;
        let original_command = commands
            .iter()
            .find(|command| command.id == command_id)
            .expect("original implementation command should remain visible");
        assert_eq!(original_command.status, "cancelled");
        let jobs = store.runtime_jobs_for_command(&command_id).await?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].status,
            harness_workflow::runtime::RuntimeJobStatus::Cancelled
        );
        assert!(jobs[0].lease.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cancel_prompt_submission_cancels_dispatched_runtime_jobs() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("runtime-prompt-handle-cancel");
        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "cancel this prompt runtime task",
                depends_on: &[],
                serialization_depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;
        let command_id = result
            .command_ids
            .first()
            .expect("prompt submission should enqueue an implementation command")
            .clone();
        store
            .enqueue_runtime_job_for_pending_command(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexExec,
                "default",
                json!({ "task_id": task_id.as_str() }),
                None,
            )
            .await?;
        assert_eq!(
            store
                .get_command(&command_id)
                .await?
                .expect("command should exist")
                .status,
            "dispatched"
        );

        let cancelled = cancel_issue_submission_by_task_id(&store, &task_id)
            .await?
            .expect("runtime prompt submission should resolve by task id");
        assert_eq!(cancelled.state, "cancelled");

        let commands = store.commands_for(&result.workflow_id).await?;
        let original_command = commands
            .iter()
            .find(|command| command.id == command_id)
            .expect("original implementation command should remain visible");
        assert_eq!(original_command.status, "cancelled");
        let jobs = store.runtime_jobs_for_command(&command_id).await?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].status,
            harness_workflow::runtime::RuntimeJobStatus::Cancelled
        );
        assert!(jobs[0].lease.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_issue_submission_keeps_existing_runtime_data() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy().into_owned();
        let workflow_id =
            harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 42);
        let mut existing = issue_instance(
            workflow_id.clone(),
            project_id,
            Some("owner/repo".to_string()),
            42,
        );
        existing.state = "pr_open".to_string();
        existing.data = serde_json::json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 42,
            "task_id": "older-task",
            "pr_url": "https://github.com/owner/repo/pull/99",
            "last_decision": "bind_pr"
        });
        let original_data = existing.data.clone();
        store.upsert_instance(&existing).await?;

        let task_id = TaskId::from_str("new-task");
        let labels = vec!["bug".to_string()];
        let result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 42,
                task_id: &task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: Some("do not clobber existing metadata"),
                depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;

        assert!(!result.accepted);
        assert!(result.command_ids.is_empty());
        assert!(result
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("TransitionNotAllowed")));

        let persisted = store
            .get_instance(&workflow_id)
            .await?
            .expect("existing workflow should remain persisted");
        assert_eq!(persisted.state, "pr_open");
        assert_eq!(persisted.data, original_data);
        assert!(store.commands_for(&workflow_id).await?.is_empty());

        let decisions = store.decisions_for(&workflow_id).await?;
        assert_eq!(decisions.len(), 1);
        assert!(!decisions[0].accepted);
        assert!(decisions[0]
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("TransitionNotAllowed")));

        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "IssueSubmitted"));
        Ok(())
    }

    #[tokio::test]
    async fn issue_submission_waits_for_dependencies_then_releases_runtime_command(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let dep_id = TaskId::from_str("dep-1");
        let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Pending;
        task_store.insert(&dep).await;

        let task_id = TaskId::from_str("runtime-handle-1");
        let labels = vec!["bug".to_string()];
        let result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 77,
                task_id: &task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: std::slice::from_ref(&dep_id),
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        assert!(result.accepted);
        assert!(result.command_ids.is_empty());
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should be persisted");
        assert_eq!(workflow.state, "awaiting_dependencies");
        assert_eq!(workflow.data["depends_on"], serde_json::json!(["dep-1"]));
        assert!(store.commands_for(&result.workflow_id).await?.is_empty());

        let waiting = release_ready_issue_dependencies(&store, &task_store, 10).await?;
        assert_eq!(waiting.waiting, 1);
        assert_eq!(waiting.released, 0);

        dep.status = TaskStatus::Done;
        task_store.insert(&dep).await;
        let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 1);
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        assert_eq!(workflow.state, "implementing");
        assert_eq!(workflow.data["dependencies_blocked"], false);
        let commands = store.commands_for(&result.workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
        Ok(())
    }

    #[tokio::test]
    async fn prompt_submission_waits_for_dependencies_then_releases_runtime_command(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let dep_id = TaskId::from_str("prompt-dep-1");
        let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Pending;
        task_store.insert(&dep).await;

        let task_id = TaskId::from_str("runtime-prompt-handle-1");
        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "wait until dependency is done",
                depends_on: std::slice::from_ref(&dep_id),
                serialization_depends_on: &[],
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        assert!(result.accepted);
        assert!(result.command_ids.is_empty());
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should be persisted");
        assert_eq!(workflow.state, "awaiting_dependencies");
        assert_eq!(
            workflow.data["depends_on"],
            serde_json::json!(["prompt-dep-1"])
        );
        assert!(store.commands_for(&result.workflow_id).await?.is_empty());

        let waiting = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
        assert_eq!(waiting.waiting, 1);
        assert_eq!(waiting.released, 0);

        dep.status = TaskStatus::Done;
        task_store.insert(&dep).await;
        let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 1);
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        assert_eq!(workflow.state, "implementing");
        assert_eq!(workflow.data["dependencies_blocked"], false);
        let commands = store.commands_for(&result.workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(
            commands[0].command.activity_name(),
            Some("implement_prompt")
        );
        Ok(())
    }

    #[tokio::test]
    async fn prompt_submission_releases_after_failed_serialization_dependency() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let dep_id = TaskId::from_str("prompt-serialization-dep");
        let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Failed;
        task_store.insert(&dep).await;

        let task_id = TaskId::from_str("runtime-prompt-serialized");
        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "run after serialized predecessor reaches terminal state",
                depends_on: &[],
                serialization_depends_on: std::slice::from_ref(&dep_id),
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should be persisted");
        assert_eq!(workflow.state, "awaiting_dependencies");
        assert_eq!(
            workflow.data["depends_on"],
            serde_json::json!(["prompt-serialization-dep"])
        );
        assert_eq!(
            workflow.data["serialization_depends_on"],
            serde_json::json!(["prompt-serialization-dep"])
        );

        let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 1);
        assert_eq!(released.failed, 0);
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        assert_eq!(workflow.state, "implementing");
        assert_eq!(workflow.data["dependencies_blocked"], false);
        assert_eq!(store.commands_for(&result.workflow_id).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn prompt_submission_fails_release_when_in_memory_prompt_is_missing() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let dep_id = TaskId::from_str("prompt-missing-dep");
        let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        task_store.insert(&dep).await;

        let task_id = TaskId::from_str("runtime-prompt-missing-cache");
        let result = record_prompt_submission(
            &store,
            PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "lost after server restart",
                depends_on: std::slice::from_ref(&dep_id),
                serialization_depends_on: &[],
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should be persisted");
        let prompt_ref = workflow.data["prompt_ref"]
            .as_str()
            .expect("prompt ref should be persisted")
            .to_string();
        remove_prompt_submission_prompt(Some(&prompt_ref));

        let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 0);
        assert_eq!(released.failed, 1);
        let workflow = store
            .get_instance(&result.workflow_id)
            .await?
            .expect("workflow should remain persisted");
        assert_eq!(workflow.state, "failed");
        assert_eq!(workflow.data["failure_reason"], "prompt unavailable");
        let commands = store.commands_for(&result.workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].command.command_type,
            WorkflowCommandType::MarkFailed
        );
        Ok(())
    }

    #[tokio::test]
    async fn issue_submission_releases_dependency_on_completed_runtime_handle() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let labels = vec!["bug".to_string()];
        let dep_id = TaskId::from_str("runtime-dep-handle");
        let dep_result = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 78,
                task_id: &dep_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                source: None,
                external_id: None,
            },
        )
        .await?;
        let mut dep_workflow = store
            .get_instance(&dep_result.workflow_id)
            .await?
            .expect("dependency workflow should exist");
        dep_workflow.state = "done".to_string();
        store.upsert_instance(&dep_workflow).await?;

        let task_id = TaskId::from_str("runtime-dependent-handle");
        let blocked = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 79,
                task_id: &task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: std::slice::from_ref(&dep_id),
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 1);
        let workflow = store
            .get_instance(&blocked.workflow_id)
            .await?
            .expect("dependent workflow should remain persisted");
        assert_eq!(workflow.state, "implementing");
        assert_eq!(store.commands_for(&blocked.workflow_id).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn dependency_release_rotates_waiting_rows_to_prevent_starvation() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = open_runtime_store(dir.path()).await?;
        let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let labels = vec!["bug".to_string()];

        let old_dep_id = TaskId::from_str("old-blocked-dep");
        let old_dep = crate::task_runner::TaskState::new(old_dep_id.clone());
        task_store.insert(&old_dep).await;
        let old_task_id = TaskId::from_str("old-waiting-handle");
        record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 80,
                task_id: &old_task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: std::slice::from_ref(&old_dep_id),
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let ready_dep_id = TaskId::from_str("ready-dep");
        let mut ready_dep = crate::task_runner::TaskState::new(ready_dep_id.clone());
        ready_dep.status = TaskStatus::Done;
        task_store.insert(&ready_dep).await;
        let ready_task_id = TaskId::from_str("ready-waiting-handle");
        let ready = record_issue_submission(
            &store,
            IssueSubmissionRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 81,
                task_id: &ready_task_id,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: std::slice::from_ref(&ready_dep_id),
                dependencies_blocked: true,
                source: None,
                external_id: None,
            },
        )
        .await?;

        let first = release_ready_issue_dependencies(&store, &task_store, 1).await?;
        assert_eq!(first.waiting, 1);
        assert_eq!(first.released, 0);
        let second = release_ready_issue_dependencies(&store, &task_store, 1).await?;
        assert_eq!(second.released, 1);
        let workflow = store
            .get_instance(&ready.workflow_id)
            .await?
            .expect("ready workflow should remain persisted");
        assert_eq!(workflow.state, "implementing");
        Ok(())
    }
}
