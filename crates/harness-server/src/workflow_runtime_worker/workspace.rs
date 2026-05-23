use crate::http::AppState;
use crate::task_runner::TaskId;
use harness_core::config::workflow::WorkflowDocument;
use harness_workflow::runtime::{
    RuntimeJob, WorkflowCommandRecord, WorkflowInstance, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
    REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{timeout, Duration as TokioDuration};

use super::data_helpers::activity_name;

pub(super) struct PreparedRuntimeWorkspace {
    pub run_project: PathBuf,
    pub task_id: Option<TaskId>,
    pub after_run_hook: Option<String>,
    pub before_remove_hook: Option<String>,
    pub hook_timeout_secs: u64,
    pub finish_action: RuntimeWorkspaceFinishAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeWorkspaceFinishAction {
    Remove,
    Release,
}

pub(super) async fn prepare_runtime_workspace(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    source_project_root: &Path,
    workflow_document: &WorkflowDocument,
) -> anyhow::Result<PreparedRuntimeWorkspace> {
    validate_workspace_cleanup_policy(&workflow_document.config.workspace.cleanup)?;
    match workflow_document.config.workspace.strategy.as_str() {
        "worktree" => {}
        "source" => {
            if let Some(hook) = workflow_document.config.hooks.before_run.as_deref() {
                run_workflow_hook(
                    "before_run",
                    hook,
                    source_project_root,
                    workflow_document.config.hooks.timeout_secs,
                )
                .await?;
            }
            return Ok(PreparedRuntimeWorkspace {
                run_project: source_project_root.to_path_buf(),
                task_id: None,
                after_run_hook: workflow_document.config.hooks.after_run.clone(),
                before_remove_hook: None,
                hook_timeout_secs: workflow_document.config.hooks.timeout_secs,
                finish_action: RuntimeWorkspaceFinishAction::Release,
            });
        }
        strategy => anyhow::bail!("unsupported workflow workspace strategy: {strategy}"),
    }

    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        anyhow::bail!("workflow runtime workspace manager is unavailable");
    };
    let task_id = stable_runtime_workspace_task_id(job, workflow);
    let external_id = workflow.map(|workflow| workflow.subject.subject_key.as_str());
    let repo = workflow
        .and_then(|workflow| workflow.data.get("repo"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| job.input.get("repo").and_then(serde_json::Value::as_str))
        .or(workflow_document.config.source.repo.as_deref());
    let reuse_existing_workspace = workflow_document.config.workspace.reuse_existing_workspace;
    let options = crate::workspace::WorkspaceCreateOptions {
        require_remote_head: workflow_document.config.base.require_remote_head,
        reuse_existing_workspace,
        after_create_hook: workflow_document.config.hooks.after_create.clone(),
        hook_timeout_secs: Some(workflow_document.config.hooks.timeout_secs),
        branch_prefix: workflow_document.config.workspace.branch_prefix.clone(),
        runtime_workflow_id: workflow.map(|workflow| workflow.id.clone()),
    };
    let lease = workspace_mgr
        .create_workspace_with_options(
            &task_id,
            source_project_root,
            &workflow_document.config.base.remote,
            &workflow_document.config.base.branch,
            1,
            external_id,
            repo,
            options,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

    if let Some(hook) = workflow_document.config.hooks.before_run.as_deref() {
        if let Err(error) = run_workflow_hook(
            "before_run",
            hook,
            &lease.workspace_path,
            workflow_document.config.hooks.timeout_secs,
        )
        .await
        {
            if let Some(hook) = workflow_document.config.hooks.before_remove.as_deref() {
                if let Err(remove_hook_error) = run_workflow_hook(
                    "before_remove",
                    hook,
                    &lease.workspace_path,
                    workflow_document.config.hooks.timeout_secs,
                )
                .await
                {
                    tracing::warn!(
                        runtime_job_id = %job.id,
                        workspace_path = %lease.workspace_path.display(),
                        "before_remove hook failed during before_run cleanup: {remove_hook_error}"
                    );
                }
            }
            if let Err(cleanup_error) = workspace_mgr.remove_workspace(&task_id).await {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %lease.workspace_path.display(),
                    "failed to clean up workspace after before_run hook failure: {cleanup_error}"
                );
            }
            return Err(error);
        }
    }

    Ok(PreparedRuntimeWorkspace {
        run_project: lease.workspace_path,
        task_id: Some(task_id),
        after_run_hook: workflow_document.config.hooks.after_run.clone(),
        before_remove_hook: workflow_document.config.hooks.before_remove.clone(),
        hook_timeout_secs: workflow_document.config.hooks.timeout_secs,
        finish_action: runtime_workspace_finish_action(
            &workflow_document.config.workspace.cleanup,
            reuse_existing_workspace,
            job,
            workflow,
        ),
    })
}

pub(super) async fn finish_runtime_workspace(
    state: &Arc<AppState>,
    workspace: &PreparedRuntimeWorkspace,
) -> anyhow::Result<()> {
    let hook_result = if let Some(hook) = workspace.after_run_hook.as_deref() {
        run_workflow_hook(
            "after_run",
            hook,
            &workspace.run_project,
            workspace.hook_timeout_secs,
        )
        .await
    } else {
        Ok(())
    };

    let Some(task_id) = workspace.task_id.as_ref() else {
        return hook_result;
    };
    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        return hook_result;
    };
    if workspace.finish_action == RuntimeWorkspaceFinishAction::Remove {
        if let Some(hook) = workspace.before_remove_hook.as_deref() {
            if let Err(error) = run_workflow_hook(
                "before_remove",
                hook,
                &workspace.run_project,
                workspace.hook_timeout_secs,
            )
            .await
            {
                tracing::warn!(
                    workspace_path = %workspace.run_project.display(),
                    "before_remove hook failed during runtime workspace cleanup: {error}"
                );
            }
        }
        workspace_mgr.remove_workspace(task_id).await?;
    } else {
        workspace_mgr.release_workspace(task_id);
    }
    hook_result
}

fn validate_workspace_cleanup_policy(cleanup: &str) -> anyhow::Result<()> {
    match cleanup {
        "after_run" | "on_terminal" => Ok(()),
        cleanup => anyhow::bail!("unsupported workflow workspace cleanup policy: {cleanup}"),
    }
}

pub(super) fn runtime_workspace_finish_action(
    cleanup: &str,
    reuse_existing_workspace: bool,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> RuntimeWorkspaceFinishAction {
    if cleanup == "after_run"
        || !reuse_existing_workspace
        || runtime_workspace_activity_is_ephemeral(&activity_name(job), workflow)
    {
        RuntimeWorkspaceFinishAction::Remove
    } else {
        RuntimeWorkspaceFinishAction::Release
    }
}

fn runtime_workspace_activity_is_ephemeral(
    activity: &str,
    workflow: Option<&WorkflowInstance>,
) -> bool {
    let Some(workflow) = workflow else {
        return false;
    };
    matches!(
        (workflow.definition_id.as_str(), activity),
        (REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY)
            | (
                REPO_BACKLOG_DEFINITION_ID,
                REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
            )
            | (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY)
            | (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY)
    )
}

pub(super) fn stable_runtime_workspace_task_id(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> TaskId {
    if let Some(workflow) = workflow {
        let definition = sanitize_workspace_id_component(&workflow.definition_id);
        return TaskId::from_str(&format!(
            "runtime-wf-{definition}-{}",
            stable_hash_8(&workflow.id)
        ));
    }
    TaskId::from_str(&format!("runtime-job-{}", stable_hash_8(&job.id)))
}

fn sanitize_workspace_id_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').to_string()
}

fn stable_hash_8(value: &str) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{hash:08x}")
}

async fn run_workflow_hook(
    hook_name: &str,
    hook: &str,
    cwd: &Path,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let hook_timeout = TokioDuration::from_secs(timeout_secs.max(1));
    match timeout(hook_timeout, crate::workspace::run_hook(hook, cwd)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow::anyhow!("{hook_name} hook failed: {error}")),
        Err(_) => Err(anyhow::anyhow!(
            "{hook_name} hook timed out after {}s",
            hook_timeout.as_secs()
        )),
    }
}

pub(super) fn is_active_pr_feedback_inspect_command(record: &WorkflowCommandRecord) -> bool {
    matches!(record.status.as_str(), "pending" | "dispatched")
        && is_pr_feedback_inspect_command(record)
}

pub(super) fn is_pr_feedback_inspect_command(record: &WorkflowCommandRecord) -> bool {
    record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};
    use serde_json::json;

    #[test]
    fn stable_runtime_workspace_task_id_reuses_workflow_identity_across_jobs() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:124"),
        )
        .with_id("/repo/root::repo:owner/repo::issue:124");
        let first_job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        );
        let second_job = RuntimeJob::pending(
            "command-2",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "inspect_pr_feedback" }),
        );

        let first = stable_runtime_workspace_task_id(&first_job, Some(&workflow));
        let second = stable_runtime_workspace_task_id(&second_job, Some(&workflow));

        assert_eq!(first, second);
        assert!(first.as_str().starts_with("runtime-wf-github-issue-pr-"));
        assert!(first
            .as_str()
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
    }

    #[test]
    fn runtime_workspace_finish_action_preserves_reusable_issue_workspaces() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        );
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:124"),
        )
        .with_id("issue-124");

        assert_eq!(
            runtime_workspace_finish_action("on_terminal", true, &job, Some(&workflow)),
            RuntimeWorkspaceFinishAction::Release
        );
    }

    #[test]
    fn runtime_workspace_finish_action_removes_ephemeral_or_non_reused_workspaces() {
        let backlog_job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": REPO_BACKLOG_POLL_ACTIVITY }),
        );
        let backlog = WorkflowInstance::new(
            REPO_BACKLOG_DEFINITION_ID,
            1,
            "scanning",
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id("repo-backlog-owner-repo");
        assert_eq!(
            runtime_workspace_finish_action("on_terminal", true, &backlog_job, Some(&backlog)),
            RuntimeWorkspaceFinishAction::Remove
        );

        let issue_job = RuntimeJob::pending(
            "command-2",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "implement_issue" }),
        );
        let issue = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:124"),
        )
        .with_id("issue-124");
        assert_eq!(
            runtime_workspace_finish_action("on_terminal", false, &issue_job, Some(&issue)),
            RuntimeWorkspaceFinishAction::Remove
        );
    }
}
