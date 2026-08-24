use crate::http::AppState;
use harness_core::config::workflow::WorkflowDocument;
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    RuntimeJob, WorkflowCommandRecord, WorkflowCommandStatus, WorkflowInstance,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{timeout, Duration as TokioDuration};

use super::data_helpers::activity_name;
use crate::workspace_lease_store::RepositoryWriteLease;

pub(super) struct PreparedRuntimeWorkspace {
    pub run_project: PathBuf,
    pub task_id: Option<TaskId>,
    pub after_run_hook: Option<String>,
    pub before_remove_hook: Option<String>,
    pub hook_timeout_secs: u64,
    pub finish_action: RuntimeWorkspaceFinishAction,
    pub _repository_write_lease: Option<RepositoryWriteLease>,
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
            let repository_write_lease = match state.concurrency.workspace_mgr.as_ref() {
                Some(workspace_mgr) => {
                    acquire_runtime_repository_lease(state, workspace_mgr, source_project_root)
                        .await?
                        .0
                }
                None => anyhow::bail!(
                    "source workspace repository execution requires the PostgreSQL workspace lease store"
                ),
            };
            if repository_write_lease.is_some() {
                revalidate_runtime_workspace_admission(state, job, workflow).await?;
            }
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
                _repository_write_lease: repository_write_lease,
            });
        }
        strategy => anyhow::bail!("unsupported workflow workspace strategy: {strategy}"),
    }

    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        anyhow::bail!("workflow runtime workspace manager is unavailable");
    };
    let (repository_write_lease, workspace_capacity) =
        acquire_runtime_repository_lease(state, workspace_mgr, source_project_root).await?;
    if repository_write_lease.is_some() {
        revalidate_runtime_workspace_admission(state, job, workflow).await?;
    }
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
        workspace_capacity_override: Some(workspace_capacity),
        repository_write_lease: repository_write_lease.map_or(
            crate::workspace::RepositoryWriteLeaseInput::NotRequired,
            crate::workspace::RepositoryWriteLeaseInput::Held,
        ),
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
        _repository_write_lease: None,
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
        workspace_mgr.release_workspace(task_id).await;
    }
    hook_result
}

pub(super) async fn cleanup_terminal_runtime_workspace(
    state: &AppState,
    workflow: &WorkflowInstance,
) -> anyhow::Result<()> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(());
    };
    if !workflow.is_terminal_with_registry(store.definition_registry()) {
        return Ok(());
    }
    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        return Ok(());
    };
    let Some(project_id) = workflow
        .data
        .get("project_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    let source_project_root = PathBuf::from(project_id);
    let workflow_document =
        harness_core::config::workflow::load_workflow_document(&source_project_root)?;
    if workflow_document.config.workspace.strategy != "worktree"
        || workflow_document.config.workspace.cleanup != "on_terminal"
    {
        return Ok(());
    }

    let task_id = stable_runtime_workspace_task_id_for_workflow(workflow);
    let repo = workflow
        .data
        .get("repo")
        .and_then(serde_json::Value::as_str)
        .or(workflow_document.config.source.repo.as_deref());
    let _repository_write_lease = workspace_mgr
        .acquire_repository_write_lease_for_cleanup(&source_project_root)
        .await?;
    let current_workflow = store.get_instance(&workflow.id).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "workflow {} disappeared before workspace cleanup",
            workflow.id
        )
    })?;
    if store
        .terminal_state_for_instance(&current_workflow)
        .await?
        .is_none()
    {
        tracing::info!(
            workflow_id = %workflow.id,
            "skipping terminal workspace cleanup because the workflow reopened while waiting for the repository lease"
        );
        return Ok(());
    }
    let mut cleanup_targets = workspace_mgr
        .workspace_targets_for_runtime_workflow(&workflow.id)
        .await?;
    if cleanup_targets.is_empty() {
        let workspace_path = workspace_mgr
            .workspace_path_for_cleanup(
                &task_id,
                &source_project_root,
                Some(workflow.subject.subject_key.as_str()),
                repo,
            )
            .await;
        if workspace_path.exists() {
            workspace_mgr
                .cleanup_workspace_for_retry(&task_id, &source_project_root, Some(&workspace_path))
                .await?;
        }
        return Ok(());
    }
    for target in cleanup_targets.drain(..) {
        if workspace_mgr
            .runtime_workspace_cleanup_target_is_superseded(&target)
            .await?
        {
            workspace_mgr
                .release_runtime_workspace_cleanup_target(&target)
                .await?;
            continue;
        }
        if !target.workspace_path.exists() {
            workspace_mgr
                .release_runtime_workspace_cleanup_target(&target)
                .await?;
            continue;
        }
        if let Some(hook) = workflow_document.config.hooks.before_remove.as_deref() {
            if let Err(error) = run_workflow_hook(
                "before_remove",
                hook,
                &target.workspace_path,
                workflow_document.config.hooks.timeout_secs,
            )
            .await
            {
                tracing::warn!(
                    workflow_id = %workflow.id,
                    workspace_path = %target.workspace_path.display(),
                    "before_remove hook failed during terminal runtime workspace cleanup: {error}"
                );
            }
        }
        workspace_mgr
            .cleanup_workspace_for_retry(
                &target.task_id,
                &source_project_root,
                Some(&target.workspace_path),
            )
            .await?;
        workspace_mgr
            .release_runtime_workspace_cleanup_target(&target)
            .await?;
    }
    Ok(())
}

fn validate_workspace_cleanup_policy(cleanup: &str) -> anyhow::Result<()> {
    match cleanup {
        "after_run" | "on_terminal" => Ok(()),
        cleanup => anyhow::bail!("unsupported workflow workspace cleanup policy: {cleanup}"),
    }
}

#[cfg(test)]
pub(crate) async fn source_project_is_configured_single_writer(
    state: &AppState,
    source_project_root: &Path,
) -> anyhow::Result<bool> {
    Ok(source_project_workspace_capacity(state, source_project_root).await? == 1)
}

async fn source_project_workspace_capacity(
    state: &AppState,
    source_project_root: &Path,
) -> anyhow::Result<usize> {
    Ok(
        crate::http::builders::workspace_pool_config::build_workspace_pool_config(
            state.core.server.as_ref(),
            state.core.project_registry.as_ref(),
        )
        .await?
        .capacity_for(source_project_root),
    )
}

async fn acquire_runtime_repository_lease(
    state: &AppState,
    workspace_mgr: &crate::workspace::WorkspaceManager,
    source_project_root: &Path,
) -> anyhow::Result<(Option<RepositoryWriteLease>, usize)> {
    loop {
        let capacity = source_project_workspace_capacity(state, source_project_root).await?;
        let single_writer = capacity == 1;
        let lease = workspace_mgr
            .acquire_repository_lease_for_runtime(source_project_root, single_writer)
            .await?;
        let current_capacity =
            source_project_workspace_capacity(state, source_project_root).await?;
        if single_writer || current_capacity > 1 {
            return Ok((lease, current_capacity));
        }
        drop(lease);
        tracing::debug!(
            source_project_root = %source_project_root.display(),
            "retrying repository admission after capacity changed to single-writer"
        );
    }
}

pub(crate) async fn revalidate_runtime_workspace_admission(
    state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<()> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workflow runtime store is unavailable"))?;
    let workflow = workflow.ok_or_else(|| {
        anyhow::anyhow!("repository workspace admission requires a persisted workflow")
    })?;
    if !store.runtime_job_matches_running_lease(job).await? {
        anyhow::bail!(
            "runtime job {} lost its running lease while waiting for the repository lease",
            job.id
        );
    }
    let sources = store
        .command_sources_for_runtime_jobs(std::slice::from_ref(&job.id))
        .await?;
    if sources
        .get(&job.id)
        .is_none_or(|source| source.workflow_id != workflow.id)
    {
        anyhow::bail!(
            "runtime job {} no longer belongs to workflow {}",
            job.id,
            workflow.id
        );
    }
    let current = store
        .get_instance(&workflow.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow {} no longer exists", workflow.id))?;
    if store.terminal_state_for_instance(&current).await?.is_some() {
        anyhow::bail!(
            "workflow {} became terminal while waiting for the repository lease",
            workflow.id
        );
    }
    Ok(())
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
        (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY)
            | (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY)
    )
}

pub(super) fn stable_runtime_workspace_task_id(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> TaskId {
    if let Some(workflow) = workflow {
        if let Some(candidate_index) = runtime_candidate_index(job) {
            if let Some(issue_number) = workflow
                .data
                .get("issue_number")
                .and_then(|value| value.as_u64())
            {
                return TaskId::from_str(&format!("issue-{issue_number}-c{candidate_index}"));
            }
            let base = stable_runtime_workspace_task_id_for_workflow(workflow);
            return TaskId::from_str(&format!("{}-c{candidate_index}", base.as_str()));
        }
        return stable_runtime_workspace_task_id_for_workflow(workflow);
    }
    TaskId::from_str(&format!("runtime-job-{}", stable_hash_8(&job.id)))
}

fn runtime_candidate_index(job: &RuntimeJob) -> Option<u64> {
    let candidate = job.input.get("command")?.get("candidate")?;
    candidate
        .get("candidate_index")?
        .as_u64()
        .filter(|index| *index > 0)
}

fn stable_runtime_workspace_task_id_for_workflow(workflow: &WorkflowInstance) -> TaskId {
    let definition = sanitize_workspace_id_component(&workflow.definition_id);
    TaskId::from_str(&format!(
        "runtime-wf-{definition}-{}",
        stable_hash_8(&workflow.id)
    ))
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
    matches!(
        record.status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Deferred
            | WorkflowCommandStatus::Dispatched
    ) && is_pr_feedback_inspect_command(record)
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
    fn candidate_fanout_workspace_task_id_uses_candidate_index() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:124"),
        )
        .with_id("/repo/root::repo:owner/repo::issue:124")
        .with_server_data(json!({
            "issue_number": 124,
        }));
        let first_job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "candidate": {
                        "candidate_index": 1,
                    },
                },
            }),
        );
        let second_job = RuntimeJob::pending(
            "command-2",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "candidate": {
                        "candidate_index": 2,
                    },
                },
            }),
        );

        let first = stable_runtime_workspace_task_id(&first_job, Some(&workflow));
        let second = stable_runtime_workspace_task_id(&second_job, Some(&workflow));

        assert_eq!(first.as_str(), "issue-124-c1");
        assert_eq!(second.as_str(), "issue-124-c2");
        assert_ne!(first, second);
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
        let pr_feedback_job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": PR_FEEDBACK_INSPECT_ACTIVITY }),
        );
        let pr_feedback = WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "inspecting",
            WorkflowSubject::new("pr", "pr:124"),
        )
        .with_id("pr-feedback-124");
        assert_eq!(
            runtime_workspace_finish_action(
                "on_terminal",
                true,
                &pr_feedback_job,
                Some(&pr_feedback)
            ),
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
