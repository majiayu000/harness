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
use crate::workspace_lease_store::RepositoryLeaseState;
use crate::workspace_lease_store::RepositoryWriteLease;

#[path = "workspace_admission.rs"]
mod admission;
#[path = "workspace_terminal_cleanup.rs"]
mod terminal_cleanup;
pub(crate) use terminal_cleanup::cleanup_terminal_runtime_workspace_if_uncontended;
pub(super) use terminal_cleanup::{
    cleanup_terminal_runtime_workspace, repository_lease_loss_error, run_preparation_phase,
    run_while_repository_lease_healthy,
};

pub(super) struct PreparedRuntimeWorkspace {
    pub run_project: PathBuf,
    pub task_id: Option<TaskId>,
    pub acquisition_id: Option<String>,
    pub runtime_workflow_id: Option<String>,
    pub execution_guard: Option<crate::workspace::WorkspaceExecutionGuard>,
    pub after_run_hook: Option<String>,
    pub before_remove_hook: Option<String>,
    pub hook_timeout_secs: u64,
    pub finish_action: RuntimeWorkspaceFinishAction,
    pub repository_lease_lost: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
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
    execution_cancelled: tokio::sync::watch::Receiver<bool>,
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
                run_preparation_phase(
                    repository_write_lease
                        .as_ref()
                        .map(RepositoryWriteLease::loss_receiver),
                    execution_cancelled.clone(),
                    run_workflow_hook(
                        "before_run",
                        hook,
                        source_project_root,
                        workflow_document.config.hooks.timeout_secs,
                    ),
                    "source workspace before_run hook",
                )
                .await?;
            }
            return Ok(PreparedRuntimeWorkspace {
                run_project: source_project_root.to_path_buf(),
                task_id: None,
                acquisition_id: None,
                runtime_workflow_id: None,
                execution_guard: None,
                after_run_hook: workflow_document.config.hooks.after_run.clone(),
                before_remove_hook: None,
                hook_timeout_secs: workflow_document.config.hooks.timeout_secs,
                finish_action: RuntimeWorkspaceFinishAction::Release,
                repository_lease_lost: repository_write_lease
                    .as_ref()
                    .map(RepositoryWriteLease::loss_receiver),
                _repository_write_lease: repository_write_lease,
            });
        }
        strategy => anyhow::bail!("unsupported workflow workspace strategy: {strategy}"),
    }

    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        anyhow::bail!("workflow runtime workspace manager is unavailable");
    };
    let task_id = stable_runtime_workspace_task_id(job, workflow);
    workspace_mgr
        .cleanup_required_workspace_for_retry(
            &task_id,
            workflow_document.config.hooks.before_remove.as_deref(),
            workflow_document.config.hooks.timeout_secs,
        )
        .await?;
    workspace_mgr.discard_unhealthy_repository_lease(&task_id);
    let external_id = workflow.map(|workflow| workflow.subject.subject_key.as_str());
    let repo = workflow
        .and_then(|workflow| workflow.data.get("repo"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| job.input.get("repo").and_then(serde_json::Value::as_str))
        .or(workflow_document.config.source.repo.as_deref());
    let reuse_existing_workspace = workflow_document.config.workspace.reuse_existing_workspace;
    let lease = admission::create_runtime_worktree_with_admission(
        state,
        job,
        workflow,
        workspace_mgr,
        &task_id,
        source_project_root,
        workflow_document,
        execution_cancelled.clone(),
        external_id,
        repo,
        reuse_existing_workspace,
    )
    .await?;

    if let Some(hook) = workflow_document.config.hooks.before_run.as_deref() {
        let preparation_guard = workspace_mgr.begin_workspace_preparation(
            &task_id,
            &lease.acquisition_id,
            workflow_document.config.hooks.before_remove.clone(),
            workflow_document.config.hooks.timeout_secs,
        )?;
        if let Err(error) = run_preparation_phase(
            lease.repository_lease_lost.clone(),
            execution_cancelled.clone(),
            run_workflow_hook(
                "before_run",
                hook,
                &lease.workspace_path,
                workflow_document.config.hooks.timeout_secs,
            ),
            "worktree before_run hook",
        )
        .await
        {
            drop(preparation_guard);
            if lease
                .repository_lease_lost
                .as_ref()
                .is_some_and(|receiver| {
                    matches!(
                        *receiver.borrow(),
                        RepositoryLeaseState::Revoking | RepositoryLeaseState::Lost
                    )
                })
                || *execution_cancelled.borrow()
            {
                return Err(error);
            }
            if let Err(cleanup_error) = workspace_mgr
                .cleanup_required_workspace_for_retry(
                    &task_id,
                    workflow_document.config.hooks.before_remove.as_deref(),
                    workflow_document.config.hooks.timeout_secs,
                )
                .await
            {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %lease.workspace_path.display(),
                    "failed to clean up workspace after before_run hook failure: {cleanup_error}"
                );
            }
            return Err(error);
        }
        preparation_guard.complete()?;
    }

    let execution_guard = workspace_mgr.claim_workspace_execution(
        &task_id,
        &lease.acquisition_id,
        workflow_document.config.hooks.before_remove.clone(),
        workflow_document.config.hooks.timeout_secs,
    )?;
    if *execution_cancelled.borrow() {
        anyhow::bail!("runtime execution was cancelled after workspace preparation");
    }

    Ok(PreparedRuntimeWorkspace {
        run_project: lease.workspace_path,
        task_id: Some(task_id),
        acquisition_id: Some(lease.acquisition_id),
        runtime_workflow_id: workflow.map(|workflow| workflow.id.clone()),
        execution_guard: Some(execution_guard),
        after_run_hook: workflow_document.config.hooks.after_run.clone(),
        before_remove_hook: workflow_document.config.hooks.before_remove.clone(),
        hook_timeout_secs: workflow_document.config.hooks.timeout_secs,
        finish_action: runtime_workspace_finish_action(
            &workflow_document.config.workspace.cleanup,
            reuse_existing_workspace,
            job,
            workflow,
        ),
        repository_lease_lost: lease.repository_lease_lost,
        _repository_write_lease: None,
    })
}

pub(super) async fn finish_runtime_workspace(
    state: &Arc<AppState>,
    workspace: &PreparedRuntimeWorkspace,
) -> anyhow::Result<()> {
    if let (Some(workspace_mgr), Some(task_id), Some(acquisition_id), Some(execution_guard)) = (
        state.concurrency.workspace_mgr.as_ref(),
        workspace.task_id.as_ref(),
        workspace.acquisition_id.as_deref(),
        workspace.execution_guard.as_ref(),
    ) {
        workspace_mgr.begin_workspace_finalization(
            task_id,
            acquisition_id,
            execution_guard.execution_id(),
        )?;
    }
    let result = run_while_repository_lease_healthy(
        workspace.repository_lease_lost.clone(),
        finish_runtime_workspace_inner(state, workspace),
        "runtime workspace finalization",
    )
    .await;
    if result.is_ok() {
        if let Some(execution_guard) = workspace.execution_guard.as_ref() {
            execution_guard.complete();
        }
    }
    result
}

async fn finish_runtime_workspace_inner(
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
        let acquisition_id = workspace
            .acquisition_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("runtime workspace acquisition ID is missing"))?;
        let cleanup_operation = workspace_mgr.workspace_cleanup_operation(acquisition_id);
        workspace_mgr
            .run_workspace_cleanup_hooks_once(
                Some(&cleanup_operation),
                task_id,
                workspace.runtime_workflow_id.as_deref(),
                workspace.before_remove_hook.as_deref(),
                workspace.hook_timeout_secs,
                &workspace.run_project,
            )
            .await?;
        let removal = workspace_mgr
            .remove_workspace_acquisition_without_hook(task_id, acquisition_id)
            .await;
        if removal.is_ok() {
            workspace_mgr
                .forget_local_workspace_cleanup_operation(acquisition_id, &cleanup_operation);
        }
        removal?;
    } else {
        let acquisition_id = workspace
            .acquisition_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("runtime workspace acquisition ID is missing"))?;
        workspace_mgr
            .release_workspace_acquisition(task_id, acquisition_id)
            .await?;
    }
    hook_result
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

    #[tokio::test]
    async fn repository_lease_loss_is_a_runtime_error() {
        let (sender, mut receiver) = tokio::sync::watch::channel(RepositoryLeaseState::Healthy);
        assert!(sender.send(RepositoryLeaseState::Lost).is_ok());

        let error = repository_lease_loss_error(&mut receiver).await;

        assert!(error.to_string().contains("session was lost"));
    }

    #[tokio::test]
    async fn released_repository_lease_is_not_reported_as_loss() {
        let (sender, mut receiver) = tokio::sync::watch::channel(RepositoryLeaseState::Healthy);
        assert!(sender.send(RepositoryLeaseState::Released).is_ok());

        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            repository_lease_loss_error(&mut receiver),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn prefired_repository_loss_beats_immediate_completion() {
        let (sender, receiver) = tokio::sync::watch::channel(RepositoryLeaseState::Healthy);
        assert!(sender.send(RepositoryLeaseState::Lost).is_ok());

        let error =
            run_while_repository_lease_healthy(Some(receiver), async { Ok(()) }, "test phase")
                .await
                .expect_err("prefired loss must win over ready work");

        assert!(error.to_string().contains("not healthy"));
    }

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
