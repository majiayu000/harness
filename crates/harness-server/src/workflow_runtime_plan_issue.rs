use harness_core::types::TaskId;
use harness_workflow::runtime::{
    build_plan_issue_decision, DataProvenance, PlanIssueDecisionInput, PlanIssueWorkflowAction,
    WorkflowCommand, WorkflowCommandStatus, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDecisionTransition, WorkflowDefinition, WorkflowEvidence, WorkflowInstance,
    WorkflowRejectedDecisionTransition, WorkflowRuntimeStore, WorkflowSubject,
};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const COMMAND_STATUS_HANDLED_INLINE: WorkflowCommandStatus = WorkflowCommandStatus::HandledInline;

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
    let (instance, new_instance) = match store.get_instance(&workflow_id).await? {
        Some(instance) => (instance, false),
        None => (
            issue_instance(
                workflow_id,
                project_id.clone(),
                ctx.repo.map(ToOwned::to_owned),
                ctx.issue_number,
                "implementing",
            ),
            true,
        ),
    };
    let event_payload = json!({
        "task_id": ctx.task_id.as_str(),
        "issue_number": ctx.issue_number,
        "repo": ctx.repo,
        "reason": ctx.plan_issue,
    });

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

    let mut final_instance = instance.clone();
    final_instance.state = output.decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);
    replace_plan_issue_data(
        &mut final_instance,
        crate::workflow_runtime_policy::merge_runtime_retry_policy(
            ctx.project_root,
            json!({
                "project_id": ctx.project_root.to_string_lossy(),
                "repo": ctx.repo,
                "issue_number": ctx.issue_number,
                "task_id": ctx.task_id.as_str(),
                "plan_concern": ctx.plan_issue,
                "last_decision": output.decision.decision,
            }),
        ),
    )?;
    let record = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &instance.state,
                create_if_missing: new_instance.then_some(&instance),
                event_type: "PlanIssueRaised",
                source: "workflow_runtime_plan_issue",
                payload: event_payload.clone(),
                decision: &output.decision,
                final_instance: &final_instance,
                command_status: COMMAND_STATUS_HANDLED_INLINE,
            },
            "workflow-policy",
        )
        .await?;
    match record {
        Some(record) if record.accepted => {}
        Some(record) => {
            return Ok(PlanIssueRuntimeAction::Block {
                error: record
                    .rejection_reason
                    .unwrap_or_else(|| "plan issue decision rejected".to_string()),
            });
        }
        None => {
            let reason = "workflow state changed before plan issue transition could be committed"
                .to_string();
            let record = store
                .record_rejected_decision_transition(WorkflowRejectedDecisionTransition {
                    expected_state: &instance.state,
                    create_if_missing: None,
                    event_type: "PlanIssueRaised",
                    source: "workflow_runtime_plan_issue",
                    payload: event_payload,
                    decision: &output.decision,
                    reason: &reason,
                })
                .await?;
            if record.is_none() {
                return Ok(fallback_action(ctx));
            }
            return Ok(PlanIssueRuntimeAction::Block { error: reason });
        }
    }

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
    store
        .upsert_definition(&WorkflowDefinition::new(
            "github_issue_pr",
            1,
            "GitHub issue PR workflow",
        ))
        .await?;
    let instance = store.get_instance(&workflow_id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "replan completion task `{}` has no workflow instance `{workflow_id}`",
            task_id.as_str()
        )
    })?;
    if instance.state != "replanning" {
        anyhow::bail!(
            "replan completion task `{}` cannot advance workflow `{workflow_id}` from state `{}`",
            task_id.as_str(),
            instance.state
        );
    }
    let current_task_id = instance
        .data
        .get("task_id")
        .and_then(serde_json::Value::as_str);
    if current_task_id != Some(task_id.as_str()) {
        anyhow::bail!(
            "stale replan completion task `{}` does not match workflow `{workflow_id}` task `{}`",
            task_id.as_str(),
            current_task_id.unwrap_or("<missing>")
        );
    }
    let event_payload = json!({
        "task_id": task_id.as_str(),
        "issue_number": issue_number,
        "repo": repo,
    });
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "replan_completed",
        "implementing",
        "replan activity completed and implementation should resume",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "implement_issue",
        format!("replan-completed:{}:implement", task_id.as_str()),
    ))
    .with_evidence(WorkflowEvidence::new(
        "replan_completed",
        format!("task_id={}", task_id.as_str()),
    ))
    .high_confidence();
    let mut final_instance = instance.clone();
    final_instance.state = "implementing".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    replace_plan_issue_data(
        &mut final_instance,
        crate::workflow_runtime_policy::merge_runtime_retry_policy(
            project_root,
            json!({
                "project_id": project_id,
                "repo": repo,
                "issue_number": issue_number,
                "task_id": task_id.as_str(),
                "last_event": "ReplanCompleted",
            }),
        ),
    )?;
    let record = store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: &instance.state,
                create_if_missing: None,
                event_type: "ReplanCompleted",
                source: "workflow_runtime_plan_issue",
                payload: event_payload,
                decision: &decision,
                final_instance: &final_instance,
                command_status: WorkflowCommandStatus::Pending,
            },
            "workflow-policy",
        )
        .await?;
    require_replan_completion_record(record)
}

fn require_replan_completion_record(record: Option<WorkflowDecisionRecord>) -> anyhow::Result<()> {
    match record {
        Some(record) if record.accepted => Ok(()),
        Some(record) => anyhow::bail!(
            "replan completion transition rejected: {}",
            record
                .rejection_reason
                .unwrap_or_else(|| "unknown rejection".to_string())
        ),
        None => anyhow::bail!(
            "workflow state changed before replan completion transition could be committed"
        ),
    }
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
    .with_classified_data(
        crate::workflow_runtime_policy::merge_runtime_retry_policy(
            Path::new(&project_id),
            json!({
                "project_id": project_id,
                "repo": repo,
                "issue_number": issue_number,
            }),
        ),
        DataProvenance::Server,
    )
}

fn replace_plan_issue_data(
    instance: &mut WorkflowInstance,
    data: serde_json::Value,
) -> anyhow::Result<()> {
    instance.replace_data_with_field_provenance(data, |field| match field {
        "plan_concern" => DataProvenance::Agent,
        _ => DataProvenance::Server,
    })
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

        match action {
            PlanIssueRuntimeAction::Block { error } => {
                assert!(error.contains("PLAN_ISSUE persisted after replan"));
            }
            _ => panic!("expected PlanIssueRuntimeAction::Block"),
        }
    }

    #[test]
    fn replan_completion_rejects_stale_transition() {
        let error = match require_replan_completion_record(None) {
            Ok(()) => panic!("stale replan completion should fail"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("workflow state changed before replan completion transition"));
    }

    #[tokio::test]
    async fn replan_completed_rejects_mismatched_task_generation() -> anyhow::Result<()> {
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
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            124,
        );
        let instance = issue_instance(
            workflow_id.clone(),
            project_root.to_string_lossy().into_owned(),
            Some("owner/repo".to_string()),
            124,
            "replanning",
        )
        .with_server_data(json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 124,
            "task_id": "current-replan-task",
        }));
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &instance)
            .await?;

        let error = persist_replan_completed(
            &store,
            &project_root,
            Some("owner/repo"),
            124,
            &TaskId::from_str("stale-replan-task"),
        )
        .await
        .expect_err("a stale replan task must not advance the current generation");

        assert!(error.to_string().contains("stale-replan-task"));
        let current = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should remain");
        assert_eq!(current.state, "replanning");
        assert!(store.commands_for(&workflow_id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn replan_completed_rejects_non_replanning_state() -> anyhow::Result<()> {
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
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            125,
        );
        let instance = issue_instance(
            workflow_id.clone(),
            project_root.to_string_lossy().into_owned(),
            Some("owner/repo".to_string()),
            125,
            "planning",
        )
        .with_server_data(json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 125,
            "task_id": "current-replan-task",
        }));
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &instance)
            .await?;

        let error = persist_replan_completed(
            &store,
            &project_root,
            Some("owner/repo"),
            125,
            &TaskId::from_str("current-replan-task"),
        )
        .await
        .expect_err("replan completion must require the replanning state");

        assert!(error.to_string().contains("planning"));
        let current = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should remain");
        assert_eq!(current.state, "planning");
        assert!(store.commands_for(&workflow_id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn replan_completed_rejects_missing_workflow() -> anyhow::Result<()> {
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
        let task_id = TaskId::from_str("task-2");

        let error =
            persist_replan_completed(&store, &project_root, Some("owner/repo"), 124, &task_id)
                .await
                .expect_err("a completion without its replan generation must be rejected");

        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            124,
        );
        assert!(error.to_string().contains(task_id.as_str()));
        assert!(store.get_instance(&workflow_id).await?.is_none());
        Ok(())
    }
}
