use harness_workflow::issue_lifecycle::IssueWorkflowStore;
use harness_workflow::runtime::{
    build_merged_pr_decision, build_open_issue_without_workflow_decision,
    build_stale_active_workflow_decision, repo_backlog_workflow_id, DecisionValidator,
    MergedPrDecisionInput, OpenIssueDecisionInput, StaleWorkflowDecisionInput, ValidationContext,
    WorkflowDecision, WorkflowDecisionRecord, WorkflowDefinition, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject, REPO_BACKLOG_DEFINITION_ID,
};
use serde_json::json;
use std::path::Path;

pub(crate) struct OpenIssueRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub issue_url: Option<&'a str>,
}

pub(crate) struct MergedPrRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub detail: &'a str,
}

pub(crate) struct StaleWorkflowRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub active_task_id: Option<&'a str>,
    pub observed_state: &'a str,
    pub reason: &'a str,
}

pub(crate) async fn record_open_issue_without_workflow(
    store: Option<&WorkflowRuntimeStore>,
    ctx: OpenIssueRuntimeContext<'_>,
) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) = persist_open_issue_without_workflow(store, &ctx).await {
        tracing::warn!(
            issue = ctx.issue_number,
            repo = ctx.repo,
            "workflow runtime repo backlog open issue write failed: {error}"
        );
    }
}

pub(crate) async fn record_merged_pr(
    store: Option<&WorkflowRuntimeStore>,
    issue_workflows: Option<&IssueWorkflowStore>,
    ctx: MergedPrRuntimeContext<'_>,
) {
    if let Some(workflows) = issue_workflows {
        if let Err(error) = workflows
            .record_pr_merged(
                &ctx.project_root.to_string_lossy(),
                ctx.repo,
                ctx.pr_number,
                Some(ctx.detail),
            )
            .await
        {
            tracing::warn!(
                pr = ctx.pr_number,
                repo = ctx.repo,
                "issue workflow merged PR update failed: {error}"
            );
        }
    }

    let Some(store) = store else {
        return;
    };
    if let Err(error) = persist_merged_pr(store, &ctx).await {
        tracing::warn!(
            issue = ctx.issue_number,
            pr = ctx.pr_number,
            repo = ctx.repo,
            "workflow runtime repo backlog merged PR write failed: {error}"
        );
    }
}

pub(crate) async fn record_stale_active_workflow(
    store: Option<&WorkflowRuntimeStore>,
    ctx: StaleWorkflowRuntimeContext<'_>,
) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) = persist_stale_active_workflow(store, &ctx).await {
        tracing::warn!(
            issue = ctx.issue_number,
            repo = ctx.repo,
            "workflow runtime repo backlog recovery write failed: {error}"
        );
    }
}

async fn persist_open_issue_without_workflow(
    store: &WorkflowRuntimeStore,
    ctx: &OpenIssueRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let mut instance = load_or_repo_backlog_instance(store, ctx.project_root, ctx.repo).await?;
    instance.data = merge_repo_data(
        instance.data,
        ctx.project_root,
        ctx.repo,
        json!({
            "last_issue_number": ctx.issue_number,
            "last_issue_url": ctx.issue_url,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            "IssueDiscovered",
            "workflow_runtime_repo_backlog",
            json!({
                "repo": ctx.repo,
                "issue_number": ctx.issue_number,
                "issue_url": ctx.issue_url,
            }),
        )
        .await?;
    let output = build_open_issue_without_workflow_decision(
        &instance,
        OpenIssueDecisionInput {
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            issue_url: ctx.issue_url,
        },
    );
    apply_decision(store, instance, output.decision, Some(event.id)).await
}

async fn persist_merged_pr(
    store: &WorkflowRuntimeStore,
    ctx: &MergedPrRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let mut instance = load_or_repo_backlog_instance(store, ctx.project_root, ctx.repo).await?;
    instance.data = merge_repo_data(
        instance.data,
        ctx.project_root,
        ctx.repo,
        json!({
            "last_issue_number": ctx.issue_number,
            "last_pr_number": ctx.pr_number,
            "last_pr_url": ctx.pr_url,
            "last_reconciliation_detail": ctx.detail,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            "PrMerged",
            "workflow_runtime_repo_backlog",
            json!({
                "repo": ctx.repo,
                "issue_number": ctx.issue_number,
                "pr_number": ctx.pr_number,
                "pr_url": ctx.pr_url,
                "detail": ctx.detail,
            }),
        )
        .await?;
    let output = build_merged_pr_decision(
        &instance,
        MergedPrDecisionInput {
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
        },
    );
    apply_decision(store, instance, output.decision, Some(event.id)).await
}

async fn persist_stale_active_workflow(
    store: &WorkflowRuntimeStore,
    ctx: &StaleWorkflowRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let mut instance = load_or_repo_backlog_instance(store, ctx.project_root, ctx.repo).await?;
    instance.data = merge_repo_data(
        instance.data,
        ctx.project_root,
        ctx.repo,
        json!({
            "last_issue_number": ctx.issue_number,
            "last_active_task_id": ctx.active_task_id,
            "last_observed_state": ctx.observed_state,
            "last_recovery_reason": ctx.reason,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            "RecoveryRequested",
            "workflow_runtime_repo_backlog",
            json!({
                "repo": ctx.repo,
                "issue_number": ctx.issue_number,
                "active_task_id": ctx.active_task_id,
                "observed_state": ctx.observed_state,
                "reason": ctx.reason,
            }),
        )
        .await?;
    let output = build_stale_active_workflow_decision(
        &instance,
        StaleWorkflowDecisionInput {
            repo: ctx.repo,
            issue_number: ctx.issue_number,
            active_task_id: ctx.active_task_id,
            observed_state: ctx.observed_state,
            reason: ctx.reason,
        },
    );
    apply_decision(store, instance, output.decision, Some(event.id)).await
}

async fn load_or_repo_backlog_instance(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    repo: Option<&str>,
) -> anyhow::Result<WorkflowInstance> {
    upsert_repo_backlog_definition(store).await?;
    let project_id = project_root.to_string_lossy().into_owned();
    let workflow_id = repo_backlog_workflow_id(&project_id, repo);
    Ok(match store.get_instance(&workflow_id).await? {
        Some(instance) => instance,
        None => WorkflowInstance::new(
            REPO_BACKLOG_DEFINITION_ID,
            1,
            "idle",
            WorkflowSubject::new("repo", repo.unwrap_or("<none>")),
        )
        .with_id(workflow_id)
        .with_data(crate::workflow_runtime_policy::merge_runtime_retry_policy(
            project_root,
            json!({
                "project_id": project_id,
                "repo": repo,
            }),
        )),
    })
}

async fn apply_decision(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    decision: WorkflowDecision,
    event_id: Option<String>,
) -> anyhow::Result<()> {
    let validation = DecisionValidator::repo_backlog().validate(
        &instance,
        &decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), event_id),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(decision, event_id, &reason);
            store.record_decision(&record).await?;
            return Ok(());
        }
    };
    store.record_decision(&record).await?;
    for command in &decision.commands {
        store
            .enqueue_command(&instance.id, Some(&record.id), command)
            .await?;
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(instance.data, &decision.decision);
    store.upsert_instance(&instance).await
}

async fn upsert_repo_backlog_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            REPO_BACKLOG_DEFINITION_ID,
            1,
            "Repo backlog workflow",
        ))
        .await
}

fn merge_repo_data(
    mut data: serde_json::Value,
    project_root: &Path,
    repo: Option<&str>,
    update: serde_json::Value,
) -> serde_json::Value {
    let project_id = project_root.to_string_lossy().into_owned();
    if let Some(object) = data.as_object_mut() {
        object.insert("project_id".to_string(), json!(project_id));
        object.insert("repo".to_string(), json!(repo));
        if let Some(update_object) = update.as_object() {
            for (key, value) in update_object {
                object.insert(key.clone(), value.clone());
            }
        }
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(project_root, data)
}

fn merge_last_decision(mut data: serde_json::Value, decision: &str) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;
    use harness_workflow::issue_lifecycle::IssueLifecycleState;

    async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
        let database_url = resolve_database_url(None)?;
        WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
    }

    async fn open_issue_store(dir: &Path) -> anyhow::Result<IssueWorkflowStore> {
        let database_url = resolve_database_url(None)?;
        IssueWorkflowStore::open_with_database_url(dir, Some(&database_url)).await
    }

    #[tokio::test]
    async fn open_issue_without_workflow_emits_start_command() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = match open_runtime_store(dir.path()).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;

        record_open_issue_without_workflow(
            Some(&store),
            OpenIssueRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 42,
                issue_url: Some("https://github.com/owner/repo/issues/42"),
            },
        )
        .await;

        let workflow_id =
            repo_backlog_workflow_id(&project_root.to_string_lossy(), Some("owner/repo"));
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("repo backlog workflow instance should be persisted");
        assert_eq!(instance.state, "dispatching");
        assert_eq!(
            store.events_for(&workflow_id).await?[0].event_type,
            "IssueDiscovered"
        );
        Ok(())
    }

    #[tokio::test]
    async fn merged_pr_updates_bound_issue_workflow() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let runtime_store = match open_runtime_store(&dir.path().join("runtime")).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
        let issue_store = match open_issue_store(&dir.path().join("issue")).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy();
        issue_store
            .record_issue_scheduled(&project_id, Some("owner/repo"), 42, "task-1", &[], false)
            .await?;
        issue_store
            .record_pr_detected(
                &project_id,
                Some("owner/repo"),
                42,
                "task-1",
                77,
                "https://github.com/owner/repo/pull/77",
            )
            .await?;

        record_merged_pr(
            Some(&runtime_store),
            Some(&issue_store),
            MergedPrRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: Some(42),
                pr_number: 77,
                pr_url: Some("https://github.com/owner/repo/pull/77"),
                detail: "reconciled: PR merged externally",
            },
        )
        .await;

        let issue_workflow = issue_store
            .get_by_issue(&project_id, Some("owner/repo"), 42)
            .await?
            .expect("issue workflow should exist");
        assert_eq!(issue_workflow.state, IssueLifecycleState::Done);

        let backlog_id = repo_backlog_workflow_id(&project_id, Some("owner/repo"));
        let events = runtime_store.events_for(&backlog_id).await?;
        assert!(events.iter().any(|event| event.event_type == "PrMerged"));
        Ok(())
    }

    #[tokio::test]
    async fn stale_active_workflow_emits_recovery_event() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = match open_runtime_store(dir.path()).await {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;

        record_stale_active_workflow(
            Some(&store),
            StaleWorkflowRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 42,
                active_task_id: Some("task-1"),
                observed_state: "implementing",
                reason: "active task disappeared during startup reconciliation",
            },
        )
        .await;

        let workflow_id =
            repo_backlog_workflow_id(&project_root.to_string_lossy(), Some("owner/repo"));
        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "RecoveryRequested"));
        Ok(())
    }
}
