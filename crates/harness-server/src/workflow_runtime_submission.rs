use crate::task_runner::{TaskId, TaskStatus, TaskStore};
use harness_workflow::runtime::{
    activity_result_has_closed_issue_evidence, build_issue_submission_decision,
    build_prompt_submission_decision, value_has_closed_issue_evidence, ActivityResult,
    DecisionValidator, IssueSubmissionDecisionInput, PromptSubmissionDecisionInput,
    ValidationContext, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvent, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject, PROMPT_TASK_DEFINITION_ID, RUNTIME_JOB_COMPLETED_EVENT,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
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
    runtime_dependency_status_from_instance(store, &instance).await
}

async fn runtime_dependency_status_from_instance(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<RuntimeDependencyStatus> {
    Ok(match instance.state.as_str() {
        "done" => RuntimeDependencyStatus::Done,
        "failed" => RuntimeDependencyStatus::Failed,
        "cancelled" => RuntimeDependencyStatus::Cancelled,
        "blocked" if blocked_issue_dependency_is_satisfied(store, instance).await? => {
            RuntimeDependencyStatus::Done
        }
        _ => RuntimeDependencyStatus::Waiting,
    })
}

async fn blocked_issue_dependency_is_satisfied(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.state != "blocked" {
        return Ok(false);
    }
    if instance_data_has_closed_issue_evidence(&instance.data) {
        return Ok(true);
    }
    let events = store.events_for(&instance.id).await?;
    Ok(events
        .iter()
        .rev()
        .any(runtime_event_has_closed_issue_evidence))
}

fn instance_data_has_closed_issue_evidence(data: &serde_json::Value) -> bool {
    ["closed_issue_evidence", "issue_state"]
        .iter()
        .filter_map(|field| data.get(field))
        .any(value_has_closed_issue_evidence)
}

fn runtime_event_has_closed_issue_evidence(event: &WorkflowEvent) -> bool {
    if event.event_type != RUNTIME_JOB_COMPLETED_EVENT {
        return false;
    }
    event
        .event
        .get("activity_result")
        .cloned()
        .and_then(|value| serde_json::from_value::<ActivityResult>(value).ok())
        .is_some_and(|result| activity_result_has_closed_issue_evidence(&result))
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

pub(crate) async fn cancel_issue_submission_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &TaskId,
) -> Result<RuntimeSubmissionCancelOutcome, RuntimeSubmissionCancelError> {
    let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? else {
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
        format!("{command_prefix}:{correlation_id}:cancel"),
        json!({ "task_id": correlation_id }),
    ))
    .high_confidence();
    let mut cancelled = commit_runtime_decision(store, instance, decision, event.id, None).await?;
    let commands = store.commands_for(&cancelled.id).await?;
    for command in commands {
        if matches!(command.status.as_str(), "pending" | "dispatched") {
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
        remove_prompt_submission_prompt(
            optional_string_field(&cancelled.data, "prompt_ref").as_deref(),
        );
    }
    Ok(RuntimeSubmissionCancelOutcome::Cancelled(cancelled))
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
#[path = "workflow_runtime_submission_tests.rs"]
mod tests;
