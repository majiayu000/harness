use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_issue_submission_decision, DecisionValidator, IssueSubmissionDecisionInput,
    ValidationContext, WorkflowDecision, WorkflowDecisionRecord, WorkflowDefinition,
    WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
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
    let submitted_data = issue_submission_data(ctx, &project_id);
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: ctx.task_id.as_str(),
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            labels: ctx.labels,
            force_execute: ctx.force_execute,
            additional_prompt: ctx.additional_prompt,
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
) -> serde_json::Value {
    crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
            "project_id": project_id,
            "repo": ctx.repo,
            "issue_number": ctx.issue_number,
            "task_id": ctx.task_id.as_str(),
            "labels": ctx.labels,
            "force_execute": ctx.force_execute,
            "additional_prompt": ctx.additional_prompt,
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
}
