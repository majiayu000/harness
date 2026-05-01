use crate::task_runner::{TaskId, TaskStatus, TaskStore};
use harness_workflow::runtime::{
    build_issue_submission_decision, DecisionValidator, IssueSubmissionDecisionInput,
    ValidationContext, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore,
    WorkflowSubject,
};
use serde_json::json;
use std::path::Path;

const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
const EXECUTION_PATH_WORKFLOW_RUNTIME: &str = "workflow_runtime";

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueSubmissionRuntimeRecord {
    pub workflow_id: String,
    pub accepted: bool,
    pub decision_id: String,
    pub command_ids: Vec<String>,
    pub rejection_reason: Option<String>,
}

pub(crate) async fn record_issue_submission(
    store: &WorkflowRuntimeStore,
    ctx: IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<IssueSubmissionRuntimeRecord> {
    persist_issue_submission(store, &ctx).await
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IssueDependencyReleaseSummary {
    pub released: usize,
    pub failed: usize,
    pub waiting: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueDependencyStatus {
    Done,
    Failed,
    Cancelled,
    Waiting,
}

pub(crate) async fn resolve_issue_dependency_status(
    store: Option<&WorkflowRuntimeStore>,
    tasks: &TaskStore,
    task_id: &TaskId,
) -> anyhow::Result<IssueDependencyStatus> {
    match tasks.dep_status(task_id).await {
        Some(TaskStatus::Done) => return Ok(IssueDependencyStatus::Done),
        Some(TaskStatus::Failed) => return Ok(IssueDependencyStatus::Failed),
        Some(TaskStatus::Cancelled) => return Ok(IssueDependencyStatus::Cancelled),
        Some(_) => return Ok(IssueDependencyStatus::Waiting),
        None => {}
    }

    let Some(store) = store else {
        return Ok(IssueDependencyStatus::Waiting);
    };
    let Some(instance) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(IssueDependencyStatus::Waiting);
    };
    Ok(match instance.state.as_str() {
        "done" => IssueDependencyStatus::Done,
        "failed" => IssueDependencyStatus::Failed,
        "cancelled" => IssueDependencyStatus::Cancelled,
        _ => IssueDependencyStatus::Waiting,
    })
}

pub(crate) async fn release_ready_issue_dependencies(
    store: &WorkflowRuntimeStore,
    tasks: &TaskStore,
    limit: i64,
) -> anyhow::Result<IssueDependencyReleaseSummary> {
    let instances = store
        .list_instances_by_state(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_dependencies",
            limit,
        )
        .await?;
    let mut summary = IssueDependencyReleaseSummary::default();
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
                IssueDependencyStatus::Done => {}
                IssueDependencyStatus::Failed => {
                    terminal_failure = Some((dep_id.clone(), "failed"));
                    break;
                }
                IssueDependencyStatus::Cancelled => {
                    terminal_failure = Some((dep_id.clone(), "cancelled"));
                    break;
                }
                IssueDependencyStatus::Waiting => all_done = false,
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
    let event = store
        .append_event(
            &instance.id,
            "IssueSubmissionCancelled",
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
        "cancel_issue_submission",
        "cancelled",
        "operator cancelled the runtime issue submission",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!("issue-submit:{}:cancel", task_id.as_str()),
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
    Ok(Some(cancelled))
}

async fn persist_issue_submission(
    store: &WorkflowRuntimeStore,
    ctx: &IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<IssueSubmissionRuntimeRecord> {
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

async fn apply_decision(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    new_instance: bool,
    decision: WorkflowDecision,
    ctx: &IssueSubmissionRuntimeContext<'_>,
    accepted_data: serde_json::Value,
) -> anyhow::Result<IssueSubmissionRuntimeRecord> {
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
            return Ok(IssueSubmissionRuntimeRecord {
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
    Ok(IssueSubmissionRuntimeRecord {
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
    if let Err(error) =
        DecisionValidator::github_issue_pr().validate(&instance, &decision, &validation_context)
    {
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

async fn upsert_github_issue_pr_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
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
        assert_eq!(instance.state, "scheduled");
        assert_eq!(instance.data["task_id"], "task-1");
        assert_eq!(instance.data["task_ids"], serde_json::json!(["task-1"]));
        assert_eq!(
            instance.data["additional_prompt"],
            "include the regression test first"
        );
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
        assert_eq!(workflow.state, "scheduled");
        assert_eq!(workflow.data["dependencies_blocked"], false);
        let commands = store.commands_for(&result.workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
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
            },
        )
        .await?;

        let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
        assert_eq!(released.released, 1);
        let workflow = store
            .get_instance(&blocked.workflow_id)
            .await?
            .expect("dependent workflow should remain persisted");
        assert_eq!(workflow.state, "scheduled");
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
        assert_eq!(workflow.state, "scheduled");
        Ok(())
    }
}
