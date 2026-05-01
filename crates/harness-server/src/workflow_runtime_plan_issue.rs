use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_plan_issue_decision, DecisionValidator, PlanIssueDecisionInput, PlanIssueWorkflowAction,
    ValidationContext, WorkflowDecisionRecord, WorkflowDefinition, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject,
};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const COMMAND_STATUS_HANDLED_INLINE: &str = "handled_inline";

pub(crate) enum PlanIssueRuntimeAction {
    RunReplan,
    ForceContinue,
    Block { error: String },
}

pub(crate) struct PlanIssueRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub task_id: &'a TaskId,
    pub plan_issue: &'a str,
    pub force_execute: bool,
    pub auto_replan_on_plan_issue: bool,
    pub replan_already_attempted: bool,
    pub turn_budget_exhausted: bool,
}

pub(crate) async fn decide_plan_issue(
    store: Option<Arc<WorkflowRuntimeStore>>,
    ctx: PlanIssueRuntimeContext<'_>,
) -> PlanIssueRuntimeAction {
    let fallback = fallback_action(&ctx);
    let Some(store) = store else {
        return fallback;
    };

    match persist_plan_issue_decision(&store, &ctx).await {
        Ok(action) => action,
        Err(error) => {
            tracing::warn!(
                issue = ctx.issue_number,
                task_id = %ctx.task_id.0,
                "workflow runtime PLAN_ISSUE decision write failed: {error}"
            );
            fallback
        }
    }
}

pub(crate) async fn record_replan_completed(
    store: Option<Arc<WorkflowRuntimeStore>>,
    project_root: &Path,
    repo: Option<&str>,
    issue_number: u64,
    task_id: &TaskId,
) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) =
        persist_replan_completed(&store, project_root, repo, issue_number, task_id).await
    {
        tracing::warn!(
            issue = issue_number,
            task_id = %task_id.0,
            "workflow runtime ReplanCompleted write failed: {error}"
        );
    }
}

async fn persist_plan_issue_decision(
    store: &WorkflowRuntimeStore,
    ctx: &PlanIssueRuntimeContext<'_>,
) -> anyhow::Result<PlanIssueRuntimeAction> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, ctx.issue_number);
    store
        .upsert_definition(&WorkflowDefinition::new(
            "github_issue_pr",
            1,
            "GitHub issue PR workflow",
        ))
        .await?;
    let mut instance = match store.get_instance(&workflow_id).await? {
        Some(instance) => instance,
        None => issue_instance(
            workflow_id,
            project_id.clone(),
            ctx.repo.map(ToOwned::to_owned),
            ctx.issue_number,
            "implementing",
        ),
    };
    instance.data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
        "project_id": project_id,
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "task_id": ctx.task_id.as_str(),
        "plan_concern": ctx.plan_issue,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            "PlanIssueRaised",
            "workflow_runtime_plan_issue",
            json!({
                "task_id": ctx.task_id.as_str(),
                "issue_number": ctx.issue_number,
                "repo": ctx.repo,
                "reason": ctx.plan_issue,
            }),
        )
        .await?;

    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: ctx.task_id.as_str(),
            plan_issue: ctx.plan_issue,
            force_execute: ctx.force_execute,
            auto_replan_on_plan_issue: ctx.auto_replan_on_plan_issue,
            replan_already_attempted: ctx.replan_already_attempted,
            turn_budget_exhausted: ctx.turn_budget_exhausted,
        },
    );

    let validator = DecisionValidator::github_issue_pr();
    let validation = validator.validate(
        &instance,
        &output.decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event.id)),
        Err(error) => {
            let reason = error.to_string();
            let record =
                WorkflowDecisionRecord::rejected(output.decision.clone(), Some(event.id), &reason);
            store.record_decision(&record).await?;
            return Ok(PlanIssueRuntimeAction::Block { error: reason });
        }
    };
    store.record_decision(&record).await?;
    for command in &output.decision.commands {
        if command.requires_runtime_job() {
            store
                .enqueue_command_with_status(
                    &instance.id,
                    Some(&record.id),
                    command,
                    COMMAND_STATUS_HANDLED_INLINE,
                )
                .await?;
        } else {
            store
                .enqueue_command(&instance.id, Some(&record.id), command)
                .await?;
        }
    }
    instance.state = output.decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
        "project_id": ctx.project_root.to_string_lossy(),
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "task_id": ctx.task_id.as_str(),
        "plan_concern": ctx.plan_issue,
        "last_decision": output.decision.decision,
        }),
    );
    store.upsert_instance(&instance).await?;

    Ok(match output.action {
        PlanIssueWorkflowAction::RunReplan => PlanIssueRuntimeAction::RunReplan,
        PlanIssueWorkflowAction::ForceContinue => PlanIssueRuntimeAction::ForceContinue,
        PlanIssueWorkflowAction::Block => PlanIssueRuntimeAction::Block {
            error: output.decision.reason,
        },
    })
}

async fn persist_replan_completed(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    repo: Option<&str>,
    issue_number: u64,
    task_id: &TaskId,
) -> anyhow::Result<()> {
    let project_id = project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number);
    let mut instance = match store.get_instance(&workflow_id).await? {
        Some(instance) => instance,
        None => issue_instance(
            workflow_id,
            project_id.clone(),
            repo.map(ToOwned::to_owned),
            issue_number,
            "replanning",
        ),
    };
    store
        .append_event(
            &instance.id,
            "ReplanCompleted",
            "workflow_runtime_plan_issue",
            json!({
                "task_id": task_id.as_str(),
                "issue_number": issue_number,
                "repo": repo,
            }),
        )
        .await?;
    instance.state = "implementing".to_string();
    instance.version = instance.version.saturating_add(1);
    instance.data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        project_root,
        json!({
        "project_id": project_id,
        "repo": repo,
        "issue_number": issue_number,
        "task_id": task_id.as_str(),
        "last_event": "ReplanCompleted",
        }),
    );
    store.upsert_instance(&instance).await
}

fn fallback_action(ctx: &PlanIssueRuntimeContext<'_>) -> PlanIssueRuntimeAction {
    if ctx.replan_already_attempted {
        return PlanIssueRuntimeAction::Block {
            error: format!("PLAN_ISSUE persisted after replan: {}", ctx.plan_issue),
        };
    }
    if ctx.force_execute {
        return PlanIssueRuntimeAction::ForceContinue;
    }
    if !ctx.auto_replan_on_plan_issue {
        return PlanIssueRuntimeAction::Block {
            error: format!(
                "PLAN_ISSUE encountered and auto_replan_on_plan_issue=false: {}",
                ctx.plan_issue
            ),
        };
    }
    if ctx.turn_budget_exhausted {
        return PlanIssueRuntimeAction::Block {
            error: "Turn budget exhausted before replan".to_string(),
        };
    }
    PlanIssueRuntimeAction::RunReplan
}

fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(crate::workflow_runtime_policy::merge_runtime_retry_policy(
        Path::new(&project_id),
        json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;
    use harness_workflow::runtime::{
        RuntimeCommandDispatcher, RuntimeKind, RuntimeProfile, WorkflowCommandType,
    };

    #[tokio::test]
    async fn plan_issue_decision_persists_replan_command() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => Arc::new(store),
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        std::fs::write(
            project_root.join("WORKFLOW.md"),
            r#"---
runtime_retry_policy:
  max_failed_activity_retries: 1
  activity_retries:
    replan_issue:
      max_failed_activity_retries: 2
---

Workflow policy
"#,
        )?;
        let task_id = TaskId::from_str("task-1");

        let action = decide_plan_issue(
            Some(store.clone()),
            PlanIssueRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 123,
                task_id: &task_id,
                plan_issue: "plan missed rollback",
                force_execute: false,
                auto_replan_on_plan_issue: true,
                replan_already_attempted: false,
                turn_budget_exhausted: false,
            },
        )
        .await;

        assert!(matches!(action, PlanIssueRuntimeAction::RunReplan));
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should be persisted");
        assert_eq!(instance.state, "replanning");
        assert_eq!(
            instance.data["runtime_retry_policy"]["max_failed_activity_retries"],
            1
        );
        assert_eq!(
            instance.data["runtime_retry_policy"]["activity_retries"]["replan_issue"]
                ["max_failed_activity_retries"],
            2
        );
        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "PlanIssueRaised"));
        let commands = store.commands_for(&workflow_id).await?;
        let replan_command = commands
            .iter()
            .find(|command| command.command.command_type == WorkflowCommandType::EnqueueActivity)
            .expect("replan activity command should be recorded");
        assert_eq!(replan_command.status, COMMAND_STATUS_HANDLED_INLINE);

        let dispatcher = RuntimeCommandDispatcher::new(
            &store,
            RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
        );
        dispatcher.dispatch_pending().await?;
        assert!(
            store
                .runtime_jobs_for_command(&replan_command.id)
                .await?
                .is_empty(),
            "inline replan command should not be dispatched again"
        );
        Ok(())
    }

    #[tokio::test]
    async fn plan_issue_decision_blocks_without_store_after_replan() {
        let task_id = TaskId::from_str("task-1");
        let action = decide_plan_issue(
            None,
            PlanIssueRuntimeContext {
                project_root: std::path::Path::new("/tmp/project"),
                repo: Some("owner/repo"),
                issue_number: 123,
                task_id: &task_id,
                plan_issue: "still invalid",
                force_execute: false,
                auto_replan_on_plan_issue: true,
                replan_already_attempted: true,
                turn_budget_exhausted: false,
            },
        )
        .await;

        assert!(matches!(action, PlanIssueRuntimeAction::Block { .. }));
    }
}
