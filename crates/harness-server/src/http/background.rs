use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{state::AppState, task_routes};
use crate::task_runner;
use harness_workflow::issue_lifecycle::{
    is_feedback_claim_placeholder, workflow_id, IssueLifecycleState, IssueWorkflowInstance,
};
use harness_workflow::runtime::{
    CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeKind, RuntimeProfile,
    RuntimeProfileSelector, WorkflowCommandRecord, WorkflowInstance, WorkflowRuntimeStore,
};

fn parse_issue_pr(task: &task_runner::TaskState) -> (Option<u64>, Option<u64>) {
    let (issue_from_external_id, pr_from_external_id) = task
        .external_id
        .as_deref()
        .map(|eid| {
            if let Some(n) = eid.strip_prefix("issue:") {
                (n.parse::<u64>().ok(), None)
            } else if let Some(n) = eid.strip_prefix("pr:") {
                (None, n.parse::<u64>().ok())
            } else {
                (None, None)
            }
        })
        .unwrap_or((None, None));

    let (issue_from_settings, pr_from_settings) = task
        .request_settings
        .as_ref()
        .map(|settings| (settings.issue, settings.pr))
        .unwrap_or((None, None));

    let issue = issue_from_external_id
        .or(task.issue)
        .or(issue_from_settings)
        .or_else(|| {
            if matches!(task.task_kind, task_runner::TaskKind::Issue) {
                parse_description_number(task.description.as_deref(), "issue #")
            } else {
                None
            }
        });
    let pr = pr_from_external_id.or(pr_from_settings).or_else(|| {
        if matches!(task.task_kind, task_runner::TaskKind::Pr) {
            parse_description_number(task.description.as_deref(), "PR #")
        } else {
            None
        }
    });

    (issue, pr)
}

fn parse_description_number(description: Option<&str>, prefix: &str) -> Option<u64> {
    description
        .and_then(|text| text.strip_prefix(prefix))
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

fn build_recovered_request(
    task: &task_runner::TaskState,
    canonical: std::path::PathBuf,
    issue: Option<u64>,
    pr: Option<u64>,
) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest {
        issue,
        pr,
        project: Some(canonical),
        repo: task.repo.clone(),
        source: task.source.clone(),
        external_id: task.external_id.clone(),
        parent_task_id: task.parent_id.clone(),
        priority: task.priority,
        ..Default::default()
    };
    if let Some(ref settings) = task.request_settings {
        settings.apply_to_req(&mut req);
    }
    if req.prompt.is_none() {
        if let Some(system_input) = req.system_input.as_ref() {
            req.prompt = Some(system_input.prompt().to_string());
        }
    }
    req
}

async fn record_runtime_recovery_requested(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    task: &task_runner::TaskState,
    issue_number: Option<u64>,
    recovery_kind: &'static str,
) {
    let Some(issue_number) = issue_number else {
        return;
    };
    let reason = format!("startup {recovery_kind} recovery requested redispatch");
    crate::workflow_runtime_repo_backlog::record_stale_active_workflow(
        state.core.workflow_runtime_store.as_deref(),
        crate::workflow_runtime_repo_backlog::StaleWorkflowRuntimeContext {
            project_root,
            repo: task.repo.as_deref(),
            issue_number,
            active_task_id: Some(task.id.as_str()),
            observed_state: task.status.as_ref(),
            reason: &reason,
        },
    )
    .await;
}

fn task_has_restart_safe_prompt(task: &task_runner::TaskState) -> bool {
    let mut req = task_runner::CreateTaskRequest::default();
    if let Some(ref settings) = task.request_settings {
        settings.apply_to_req(&mut req);
    }
    if req.prompt.is_none() {
        if let Some(system_input) = req.system_input.as_ref() {
            req.prompt = Some(system_input.prompt().to_string());
        }
    }
    req.prompt.is_some()
}

fn task_allows_prompt_orphan_recovery(task: &task_runner::TaskState) -> bool {
    matches!(
        task.task_kind,
        task_runner::TaskKind::Prompt
            | task_runner::TaskKind::Review
            | task_runner::TaskKind::Planner
    ) && task_has_restart_safe_prompt(task)
}

fn orphan_recovery_failure_reason(task: &task_runner::TaskState) -> &'static str {
    match task.task_kind {
        task_runner::TaskKind::Issue => "orphaned issue task: issue number not persisted",
        task_runner::TaskKind::Pr => "orphaned PR task: PR number not persisted",
        task_runner::TaskKind::Prompt => "orphaned prompt-only task: prompt not persisted",
        task_runner::TaskKind::Review => "orphaned review task: prompt not persisted",
        task_runner::TaskKind::Planner => "orphaned planner task: prompt not persisted",
    }
}

pub(super) fn recovery_queue_domain(task_kind: task_runner::TaskKind) -> task_routes::QueueDomain {
    match task_kind {
        task_runner::TaskKind::Review => task_routes::QueueDomain::Review,
        task_runner::TaskKind::Issue
        | task_runner::TaskKind::Pr
        | task_runner::TaskKind::Prompt
        | task_runner::TaskKind::Planner => task_routes::QueueDomain::Primary,
    }
}

async fn await_startup_recovery_ready_task(
    state: &Arc<AppState>,
    task_id: &crate::task_runner::TaskId,
    recovery_kind: &'static str,
) -> Option<crate::task_runner::TaskState> {
    loop {
        let task = state.core.tasks.get(task_id)?;
        if !matches!(task.status, task_runner::TaskStatus::Pending) {
            return None;
        }

        let now = chrono::Utc::now();
        if task.scheduler.has_live_runtime_host_lease(now) {
            let expires_at = task.scheduler.lease_expiry()?;
            let wait_for = expires_at
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default()
                .saturating_add(std::time::Duration::from_millis(50));
            tracing::info!(
                task_id = ?task.id,
                recovery_kind,
                owner = ?task.scheduler.runtime_host_id(),
                lease_expires_at = %expires_at,
                "startup recovery: waiting for runtime-host lease expiry before redispatch"
            );
            drop(task);
            tokio::time::sleep(wait_for).await;
            continue;
        }

        let task_id = task.id.clone();
        let Some(host_id) = task.scheduler.runtime_host_id().map(str::to_string) else {
            return Some(task);
        };
        drop(task);

        match state
            .core
            .tasks
            .release_runtime_host_claim(&task_id, &host_id)
            .await
        {
            Ok(true) => {
                tracing::info!(
                    task_id = ?task_id,
                    host_id,
                    recovery_kind,
                    "startup recovery: cleared stale runtime-host lease before redispatch"
                );
            }
            Ok(false) => {}
            Err(e) => {
                tracing::error!(
                    task_id = ?task_id,
                    host_id,
                    recovery_kind,
                    error = %e,
                    "startup recovery: failed to clear stale runtime-host lease"
                );
                return None;
            }
        }
    }
}

/// Spawn background watcher for AwaitingDeps tasks.
/// Uses Weak<AppState> to avoid a reference cycle; the loop exits when AppState is dropped.
pub(super) fn spawn_awaiting_deps_watcher(state: &Arc<AppState>) {
    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let state = match weak_state.upgrade() {
                Some(s) => s,
                None => break,
            };
            let (ready_ids, failed_ids) =
                crate::task_runner::check_awaiting_deps(&state.core.tasks).await;
            // Persist status changes for both ready and failed tasks.
            for task_id in ready_ids.iter().chain(failed_ids.iter()) {
                if let Err(e) = state.core.tasks.persist(task_id).await {
                    tracing::warn!(
                        "dep-watcher: failed to persist {} after transition: {e}",
                        task_id.0
                    );
                }
            }
            // Invoke completion_callback for dep-failed tasks so intake
            // sources (e.g. github_issues) can unmark them from the
            // dispatched map and allow retries on the next poll cycle.
            // Without this the issue stays permanently "dispatched" and
            // the intake poller never re-queues it.
            if let Some(ref cb) = state.intake.completion_callback {
                for task_id in &failed_ids {
                    if let Some(task) = state.core.tasks.get(task_id) {
                        let cb = cb.clone();
                        tokio::spawn(async move {
                            cb(task).await;
                        });
                    } else {
                        tracing::warn!(
                            "dep-watcher: task {} not found after dep-failed transition",
                            task_id.0
                        );
                    }
                }
            }
            // Spawn an agent for each task whose deps are now satisfied.
            for task_id in ready_ids {
                let state = state.clone();
                tokio::spawn(async move {
                    let task = match state.core.tasks.get(&task_id) {
                        Some(t) => t,
                        None => {
                            tracing::warn!(
                                "dep-watcher: task {} not found after Pending transition",
                                task_id.0
                            );
                            return;
                        }
                    };
                    // Reconstruct the project path: prefer stored project_root,
                    // fall back to repo slug so the agent runs in the right worktree.
                    let project_path = task
                        .project_root
                        .clone()
                        .or_else(|| task.repo.as_deref().map(std::path::PathBuf::from));
                    let canonical = match task_runner::resolve_canonical_project(project_path).await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            let reason =
                                format!("dep-watcher: failed to resolve project path: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };
                    let project_id = canonical.to_string_lossy().into_owned();

                    let (issue, pr) = parse_issue_pr(&task);
                    let req = build_recovered_request(&task, canonical, issue, pr);

                    // Guard: prompt-only tasks store their prompt in memory only
                    // (#[serde(skip)]). After a server restart the prompt field is
                    // absent. If no issue/pr is present either, dispatching would
                    // call implement_from_prompt("") — a silent mis-execution.
                    // Fail the task explicitly so the caller can re-submit.
                    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
                        let reason = format!(
                            "dep-watcher: {} task has no restart-safe input after server restart; \
                             please re-submit the task",
                            task.task_kind.as_ref()
                        );
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "dep-watcher: failed to persist failed status: {pe}"
                            );
                        }
                        // Fire completion callback so intake sources (e.g. GitHub Issues
                        // poller) remove this task from their `dispatched` map. Without
                        // this the issue stays marked as dispatched forever and will never
                        // be re-queued, causing a silent production deadlock.
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }

                    let permit = match state
                        .concurrency
                        .task_queue
                        .acquire(&project_id, task.priority)
                        .await
                    {
                        Ok(p) => p,
                        Err(e) => {
                            let reason =
                                format!("dep-watcher: failed to acquire concurrency permit: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };

                    let agent = match task_routes::select_agent(
                        &req,
                        &state.core.server.agent_registry,
                        None,
                    ) {
                        Ok(a) => a,
                        Err(e) => {
                            let reason = format!("dep-watcher: failed to select agent: {e}");
                            tracing::error!(task_id = ?task.id, "{reason}");
                            if let Err(pe) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                    s.error = Some(reason);
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "dep-watcher: failed to persist failed status: {pe}"
                                );
                            }
                            return;
                        }
                    };
                    let (reviewer, _) = super::resolve_reviewer(
                        &state.core.server.agent_registry,
                        &state.core.server.config.agents.review,
                        agent.name(),
                    );
                    state.core.tasks.register_task_stream(&task.id);
                    task_runner::spawn_preregistered_task(
                        task.id,
                        state.core.tasks.clone(),
                        agent,
                        reviewer,
                        Arc::new(state.core.server.config.clone()),
                        state.engines.skills.clone(),
                        state.observability.events.clone(),
                        state.interceptors.clone(),
                        req,
                        state.concurrency.workspace_mgr.clone(),
                        permit,
                        state.intake.completion_callback.clone(),
                        state.core.issue_workflow_store.clone(),
                        state.core.workflow_runtime_store.clone(),
                        state
                            .core
                            .server
                            .config
                            .server
                            .allowed_project_roots
                            .clone(),
                        None,
                    )
                    .await;
                });
            }
        }
    });
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimeCommandDispatchTick {
    pub enqueued: usize,
    pub already_dispatched: usize,
    pub skipped: usize,
}

impl RuntimeCommandDispatchTick {
    fn from_outcomes(outcomes: &[CommandDispatchOutcome]) -> Self {
        let mut tick = Self::default();
        for outcome in outcomes {
            match outcome {
                CommandDispatchOutcome::Enqueued { .. } => tick.enqueued += 1,
                CommandDispatchOutcome::AlreadyDispatched { .. } => tick.already_dispatched += 1,
                CommandDispatchOutcome::Skipped { .. } => tick.skipped += 1,
            }
        }
        tick
    }

    fn touched_anything(&self) -> bool {
        self.enqueued > 0 || self.already_dispatched > 0 || self.skipped > 0
    }
}

pub(super) async fn run_runtime_command_dispatch_tick(
    state: &Arc<AppState>,
    runtime_profiles: impl Into<RuntimeProfileSelector>,
    batch_limit: i64,
) -> anyhow::Result<RuntimeCommandDispatchTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeCommandDispatchTick::default());
    };
    let release = crate::workflow_runtime_submission::release_ready_issue_dependencies(
        store,
        &state.core.tasks,
        batch_limit,
    )
    .await?;
    let prompt_release = crate::workflow_runtime_submission::release_ready_prompt_dependencies(
        store,
        &state.core.tasks,
        batch_limit,
    )
    .await?;
    if release.released > 0 || release.failed > 0 || release.skipped > 0 {
        tracing::info!(
            released = release.released,
            failed = release.failed,
            waiting = release.waiting,
            skipped = release.skipped,
            "workflow runtime dependency release tick complete"
        );
    }
    if prompt_release.released > 0 || prompt_release.failed > 0 || prompt_release.skipped > 0 {
        tracing::info!(
            released = prompt_release.released,
            failed = prompt_release.failed,
            waiting = prompt_release.waiting,
            skipped = prompt_release.skipped,
            "workflow runtime prompt dependency release tick complete"
        );
    }
    let fallback_profile_selector = runtime_profiles.into();
    let commands = store.pending_commands(batch_limit).await?;
    let mut outcomes = Vec::with_capacity(commands.len());
    for command in commands {
        outcomes.push(
            dispatch_runtime_command_with_project_policy(
                state,
                store,
                command,
                fallback_profile_selector.clone(),
            )
            .await?,
        );
    }
    Ok(RuntimeCommandDispatchTick::from_outcomes(&outcomes))
}

async fn dispatch_runtime_command_with_project_policy(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    command: WorkflowCommandRecord,
    fallback_profile_selector: RuntimeProfileSelector,
) -> anyhow::Result<CommandDispatchOutcome> {
    if !command.command.requires_runtime_job() {
        return RuntimeCommandDispatcher::with_profile_selector(store, fallback_profile_selector)
            .dispatch_command(command)
            .await;
    }

    let Some(profile_selector) =
        runtime_dispatch_profile_selector_for_command(state, store, &command).await?
    else {
        let command_id = command.id.clone();
        let skipped = store
            .mark_pending_command_status(&command_id, "skipped")
            .await?;
        let reason = if skipped {
            "workflow runtime dispatch or worker is disabled for the command project".to_string()
        } else {
            "command status changed before workflow runtime policy dispatch".to_string()
        };
        return Ok(CommandDispatchOutcome::Skipped { command_id, reason });
    };

    RuntimeCommandDispatcher::with_profile_selector(store, profile_selector)
        .dispatch_command(command)
        .await
}

async fn runtime_dispatch_profile_selector_for_command(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<Option<RuntimeProfileSelector>> {
    let project_root =
        runtime_command_project_root(store, command, &state.core.project_root).await?;
    let workflow_cfg = load_runtime_workflow_config_or_default(
        &project_root,
        "workflow runtime command dispatcher",
    );
    if !workflow_cfg.runtime_dispatch.enabled || !workflow_cfg.runtime_worker.enabled {
        tracing::debug!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            project_root = %project_root.display(),
            dispatch_enabled = workflow_cfg.runtime_dispatch.enabled,
            worker_enabled = workflow_cfg.runtime_worker.enabled,
            "workflow runtime command skipped by project runtime policy"
        );
        return Ok(None);
    }
    Ok(Some(runtime_dispatch_profile_selector(
        &workflow_cfg.runtime_dispatch,
    )))
}

async fn runtime_command_project_root(
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
    fallback_project_root: &Path,
) -> anyhow::Result<PathBuf> {
    let Some(instance) = store.get_instance(&command.workflow_id).await? else {
        tracing::warn!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            fallback_project_root = %fallback_project_root.display(),
            "workflow runtime command has no workflow instance; using fallback project root"
        );
        return Ok(fallback_project_root.to_path_buf());
    };
    Ok(workflow_project_root(&instance).unwrap_or_else(|| {
        tracing::warn!(
            workflow_id = %command.workflow_id,
            command_id = %command.id,
            fallback_project_root = %fallback_project_root.display(),
            "workflow runtime command workflow has no project_id; using fallback project root"
        );
        fallback_project_root.to_path_buf()
    }))
}

fn workflow_project_root(instance: &WorkflowInstance) -> Option<PathBuf> {
    instance
        .data
        .get("project_id")
        .and_then(serde_json::Value::as_str)
        .filter(|project_id| !project_id.trim().is_empty())
        .map(PathBuf::from)
}

fn load_runtime_workflow_config_or_default(
    project_root: &Path,
    subsystem: &str,
) -> harness_core::config::workflow::WorkflowConfig {
    harness_core::config::workflow::load_workflow_config(project_root).unwrap_or_else(|e| {
        tracing::warn!(
            project_root = %project_root.display(),
            "{subsystem}: failed to load WORKFLOW.md, using default config: {e}"
        );
        harness_core::config::workflow::WorkflowConfig::default()
    })
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimeRepoBacklogPollTick {
    pub requested: usize,
    pub active_command_exists: usize,
    pub skipped: usize,
    pub rejected: usize,
}

impl RuntimeRepoBacklogPollTick {
    fn touched_anything(&self) -> bool {
        self.requested > 0
            || self.active_command_exists > 0
            || self.skipped > 0
            || self.rejected > 0
    }
}

pub(super) async fn run_runtime_repo_backlog_poll_tick(
    state: &Arc<AppState>,
    limit: usize,
) -> anyhow::Result<RuntimeRepoBacklogPollTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeRepoBacklogPollTick::default());
    };
    let Some(github_config) = state
        .core
        .server
        .config
        .intake
        .github
        .as_ref()
        .filter(|config| config.enabled)
    else {
        return Ok(RuntimeRepoBacklogPollTick::default());
    };

    let mut tick = RuntimeRepoBacklogPollTick::default();
    let mut considered_repos = 0usize;
    for repo_config in github_config.effective_repos() {
        if considered_repos >= limit.max(1) {
            break;
        }
        let project_root = repo_backlog_project_root(&repo_config, &state.core.project_root).await;
        if !project_root.exists() {
            tracing::warn!(
                repo = %repo_config.repo,
                project_root = %project_root.display(),
                "workflow runtime repo backlog poll skipped unresolvable project path"
            );
            tick.skipped += 1;
            continue;
        }
        let workflow_cfg = load_runtime_workflow_config_or_default(
            &project_root,
            "workflow runtime repo backlog poller",
        );
        if !workflow_cfg.repo_backlog.enabled
            || !workflow_cfg.runtime_dispatch.enabled
            || !workflow_cfg.runtime_worker.enabled
        {
            tick.skipped += 1;
            continue;
        }
        considered_repos += 1;
        match crate::workflow_runtime_repo_backlog::request_repo_backlog_poll(
            store,
            crate::workflow_runtime_repo_backlog::RepoBacklogPollRuntimeContext {
                project_root: &project_root,
                repo: Some(repo_config.repo.as_str()),
                label: Some(repo_config.label.as_str()),
            },
        )
        .await?
        {
            crate::workflow_runtime_repo_backlog::RepoBacklogPollRequestOutcome::Requested {
                ..
            } => tick.requested += 1,
            crate::workflow_runtime_repo_backlog::RepoBacklogPollRequestOutcome::ActiveCommandExists {
                ..
            } => tick.active_command_exists += 1,
            crate::workflow_runtime_repo_backlog::RepoBacklogPollRequestOutcome::NotCandidate {
                ..
            } => tick.skipped += 1,
            crate::workflow_runtime_repo_backlog::RepoBacklogPollRequestOutcome::Rejected {
                ..
            } => tick.rejected += 1,
        }
    }
    Ok(tick)
}

async fn repo_backlog_project_root(
    repo_config: &harness_core::config::intake::GitHubRepoConfig,
    fallback: &Path,
) -> PathBuf {
    let configured = repo_config
        .project_root
        .as_ref()
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_home_path(path))
        .unwrap_or_else(|| fallback.to_path_buf());
    match task_runner::resolve_canonical_project(Some(configured.clone())).await {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                repo = %repo_config.repo,
                project_root = %configured.display(),
                "workflow runtime repo backlog poller failed to canonicalize project root: {error}"
            );
            configured
        }
    }
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

pub(super) fn spawn_runtime_repo_backlog_poller(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime repo backlog poller disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = load_runtime_workflow_config_or_default(
                &state.core.project_root,
                "workflow runtime repo backlog poller",
            );
            let interval =
                std::time::Duration::from_secs(workflow_cfg.repo_backlog.poll_interval_secs.max(1));
            let batch_limit = workflow_cfg.repo_backlog.batch_limit.max(1) as usize;
            match run_runtime_repo_backlog_poll_tick(&state, batch_limit).await {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(
                        requested = tick.requested,
                        active_command_exists = tick.active_command_exists,
                        skipped = tick.skipped,
                        rejected = tick.rejected,
                        "workflow runtime repo backlog poller tick complete"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("workflow runtime repo backlog poller tick failed: {error}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimePrFeedbackSweepTick {
    pub requested: usize,
    pub active_command_exists: usize,
    pub skipped: usize,
    pub rejected: usize,
}

impl RuntimePrFeedbackSweepTick {
    fn touched_anything(&self) -> bool {
        self.requested > 0
            || self.active_command_exists > 0
            || self.skipped > 0
            || self.rejected > 0
    }
}

pub(super) async fn run_runtime_pr_feedback_sweep_tick(
    state: &Arc<AppState>,
    limit: usize,
) -> anyhow::Result<RuntimePrFeedbackSweepTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimePrFeedbackSweepTick::default());
    };
    let workflows = store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
        )
        .await?;
    let mut tick = RuntimePrFeedbackSweepTick::default();
    let mut considered_candidates = 0usize;
    for workflow in workflows {
        if !matches!(workflow.state.as_str(), "pr_open" | "awaiting_feedback") {
            continue;
        }
        if workflow
            .data
            .get("pr_number")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            tick.skipped += 1;
            continue;
        }
        let Some(project_root) = workflow_project_root(&workflow) else {
            tick.skipped += 1;
            continue;
        };
        if !project_root.exists() {
            tracing::warn!(
                workflow_id = %workflow.id,
                project_root = %project_root.display(),
                "workflow runtime PR feedback sweep skipped unresolvable project path"
            );
            tick.skipped += 1;
            continue;
        }
        let workflow_cfg = load_runtime_workflow_config_or_default(
            &project_root,
            "workflow runtime PR feedback sweeper",
        );
        if !workflow_cfg.pr_feedback.enabled
            || !workflow_cfg.runtime_dispatch.enabled
            || !workflow_cfg.runtime_worker.enabled
        {
            tick.skipped += 1;
            continue;
        }
        if considered_candidates >= limit.max(1) {
            break;
        }
        considered_candidates += 1;

        match crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep(store, &workflow.id)
            .await?
        {
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested {
                ..
            } => tick.requested += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                ..
            } => tick.active_command_exists += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::NotCandidate {
                ..
            } => tick.skipped += 1,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Rejected {
                ..
            } => tick.rejected += 1,
        }
    }
    Ok(tick)
}

pub(super) fn spawn_runtime_pr_feedback_sweeper(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime PR feedback sweeper disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = load_runtime_workflow_config_or_default(
                &state.core.project_root,
                "workflow runtime PR feedback sweeper",
            );
            let interval =
                std::time::Duration::from_secs(workflow_cfg.pr_feedback.sweep_interval_secs.max(1));
            match run_runtime_pr_feedback_sweep_tick(&state, 128).await {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(
                        requested = tick.requested,
                        active_command_exists = tick.active_command_exists,
                        skipped = tick.skipped,
                        rejected = tick.rejected,
                        "workflow runtime PR feedback sweeper tick complete"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("workflow runtime PR feedback sweeper tick failed: {error}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

fn runtime_kind_from_config(value: &str) -> Option<RuntimeKind> {
    match value {
        "codex_exec" => Some(RuntimeKind::CodexExec),
        "codex_jsonrpc" => Some(RuntimeKind::CodexJsonrpc),
        "claude_code" => Some(RuntimeKind::ClaudeCode),
        "anthropic_api" => Some(RuntimeKind::AnthropicApi),
        "remote_host" => Some(RuntimeKind::RemoteHost),
        _ => None,
    }
}

fn runtime_dispatch_profile(
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
) -> RuntimeProfile {
    let kind = runtime_kind_from_config(&policy.runtime_kind).unwrap_or_else(|| {
        tracing::warn!(
            runtime_kind = policy.runtime_kind,
            "workflow runtime command dispatcher: unknown runtime_kind, falling back to codex_jsonrpc"
        );
        RuntimeKind::CodexJsonrpc
    });
    let mut profile = RuntimeProfile::new(policy.runtime_profile.clone(), kind);
    profile.model = policy.model.clone();
    profile.reasoning_effort = policy.reasoning_effort.clone();
    profile.sandbox = policy.sandbox.clone();
    profile.approval_policy = runtime_kind_supports_approval_policy(kind)
        .then(|| policy.approval_policy.clone())
        .flatten();
    profile.max_turns = policy.max_turns;
    profile.timeout_secs = policy.timeout_secs;
    profile
}

fn runtime_dispatch_profile_selector(
    policy: &harness_core::config::workflow::RuntimeDispatchPolicy,
) -> RuntimeProfileSelector {
    let default_profile = runtime_dispatch_profile(policy);
    let workflow_profiles = policy
        .workflow_profiles
        .iter()
        .map(|(definition_id, override_policy)| {
            (
                definition_id.clone(),
                apply_runtime_dispatch_profile_override(&default_profile, override_policy),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let activity_profiles = policy
        .activity_profiles
        .iter()
        .map(|(activity, override_policy)| {
            (
                activity.clone(),
                apply_runtime_dispatch_profile_override(&default_profile, override_policy),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut selector = RuntimeProfileSelector::new(default_profile.clone());
    for (definition_id, profile) in &workflow_profiles {
        selector = selector.with_workflow_profile(definition_id.clone(), profile.clone());
    }
    for (activity, profile) in &activity_profiles {
        selector = selector.with_activity_profile(activity.clone(), profile.clone());
    }
    for (definition_id, activity_overrides) in &policy.workflow_activity_profiles {
        for (activity, override_policy) in activity_overrides {
            let base_profile = activity_profiles
                .get(activity)
                .or_else(|| workflow_profiles.get(definition_id))
                .unwrap_or(&default_profile);
            let profile = apply_runtime_dispatch_profile_override(base_profile, override_policy);
            selector = selector.with_workflow_activity_profile(
                definition_id.clone(),
                activity.clone(),
                profile,
            );
        }
    }
    selector
}

fn apply_runtime_dispatch_profile_override(
    default_profile: &RuntimeProfile,
    override_policy: &harness_core::config::workflow::RuntimeDispatchProfileOverride,
) -> RuntimeProfile {
    let kind = override_policy
        .runtime_kind
        .as_deref()
        .and_then(runtime_kind_from_config)
        .unwrap_or(default_profile.kind);
    let profile_name = override_policy
        .runtime_profile
        .clone()
        .unwrap_or_else(|| default_profile.name.clone());
    let mut profile = RuntimeProfile::new(profile_name, kind);
    profile.model = override_policy
        .model
        .clone()
        .or_else(|| default_profile.model.clone());
    profile.reasoning_effort = override_policy
        .reasoning_effort
        .clone()
        .or_else(|| default_profile.reasoning_effort.clone());
    profile.sandbox = override_policy
        .sandbox
        .clone()
        .or_else(|| default_profile.sandbox.clone());
    profile.approval_policy =
        runtime_dispatch_approval_policy(default_profile, override_policy, kind);
    profile.max_turns = override_policy.max_turns.or(default_profile.max_turns);
    profile.timeout_secs = override_policy
        .timeout_secs
        .or(default_profile.timeout_secs);
    profile
}

fn runtime_dispatch_approval_policy(
    default_profile: &RuntimeProfile,
    override_policy: &harness_core::config::workflow::RuntimeDispatchProfileOverride,
    kind: RuntimeKind,
) -> Option<String> {
    runtime_kind_supports_approval_policy(kind)
        .then(|| {
            override_policy
                .approval_policy
                .clone()
                .or_else(|| default_profile.approval_policy.clone())
        })
        .flatten()
}

fn runtime_kind_supports_approval_policy(kind: RuntimeKind) -> bool {
    matches!(kind, RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc)
}

pub(super) fn spawn_runtime_command_dispatcher(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime command dispatcher disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = load_runtime_workflow_config_or_default(
                &state.core.project_root,
                "workflow runtime command dispatcher",
            );
            let policy = workflow_cfg.runtime_dispatch;
            let interval = std::time::Duration::from_secs(policy.interval_secs.max(1));
            let profile_selector = runtime_dispatch_profile_selector(&policy);
            match run_runtime_command_dispatch_tick(
                &state,
                profile_selector,
                i64::from(policy.batch_limit.max(1)),
            )
            .await
            {
                Ok(tick) if tick.touched_anything() => {
                    tracing::info!(
                        enqueued = tick.enqueued,
                        already_dispatched = tick.already_dispatched,
                        skipped = tick.skipped,
                        "workflow runtime command dispatcher tick complete"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("workflow runtime command dispatcher tick failed: {e}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

fn runtime_worker_lease_ttl(
    policy: &harness_core::config::workflow::RuntimeWorkerPolicy,
) -> chrono::Duration {
    let lease_ttl_secs = i64::try_from(policy.lease_ttl_secs.max(1)).unwrap_or(i64::MAX);
    chrono::Duration::seconds(lease_ttl_secs)
}

fn runtime_worker_loop_policy(
    policy: harness_core::config::workflow::RuntimeWorkerPolicy,
) -> harness_core::config::workflow::RuntimeWorkerPolicy {
    if policy.enabled {
        policy
    } else {
        harness_core::config::workflow::RuntimeWorkerPolicy::default()
    }
}

pub(super) fn spawn_runtime_job_workers(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime job workers disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = load_runtime_workflow_config_or_default(
                &state.core.project_root,
                "workflow runtime job workers",
            );
            let policy = runtime_worker_loop_policy(workflow_cfg.runtime_worker);
            let interval = std::time::Duration::from_secs(policy.interval_secs.max(1));
            let concurrency = policy.concurrency.max(1);
            let lease_ttl = runtime_worker_lease_ttl(&policy);
            let mut handles = Vec::with_capacity(concurrency as usize);
            for worker_idx in 0..concurrency {
                let state = state.clone();
                let owner = format!("server-runtime-worker-{worker_idx}");
                handles.push(tokio::spawn(async move {
                    crate::workflow_runtime_worker::run_runtime_job_worker_tick(
                        &state, owner, lease_ttl,
                    )
                    .await
                }));
            }
            for handle in handles {
                match handle.await {
                    Ok(Ok(tick)) if tick.touched_anything() => {
                        tracing::info!(
                            succeeded = tick.succeeded,
                            failed = tick.failed,
                            cancelled = tick.cancelled,
                            "workflow runtime job worker tick complete"
                        );
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!("workflow runtime job worker tick failed: {e}");
                    }
                    Err(e) => {
                        tracing::warn!("workflow runtime job worker task panicked: {e}");
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Spawn a background sweeper that turns `pr_open` / `awaiting_feedback`
/// issue workflows into `pr:N` review tasks.
///
/// This reuses the existing PR review loop instead of adding another GitHub API
/// interpretation layer in the server. The workflow store decides *which* PRs
/// need attention; the existing `pr:N` task path decides *what* to do.
pub(super) fn spawn_issue_workflow_feedback_sweeper(state: &Arc<AppState>) {
    if state.core.issue_workflow_store.is_none() {
        tracing::debug!("workflow feedback sweeper disabled: issue workflow store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut warned_unresolvable: std::collections::HashSet<(String, Option<String>)> =
            std::collections::HashSet::new();
        loop {
            let state = match weak_state.upgrade() {
                Some(s) => s,
                None => break,
            };
            let workflow_cfg =
                harness_core::config::workflow::load_workflow_config(&state.core.project_root)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            "workflow feedback sweeper: failed to load WORKFLOW.md, using default config: {e}"
                        );
                        harness_core::config::workflow::WorkflowConfig::default()
                    });
            let interval =
                std::time::Duration::from_secs(workflow_cfg.pr_feedback.sweep_interval_secs);
            tokio::time::sleep(interval).await;

            if !workflow_cfg.pr_feedback.enabled {
                tracing::debug!("workflow feedback sweeper: disabled by WORKFLOW.md");
                continue;
            }

            let Some(issue_workflows) = state.core.issue_workflow_store.as_ref() else {
                continue;
            };

            let stale_before = chrono::Utc::now()
                - chrono::Duration::seconds(workflow_cfg.pr_feedback.claim_stale_after_secs as i64);
            let candidates = match issue_workflows
                .claim_feedback_candidates(128, stale_before)
                .await
            {
                Ok(candidates) => candidates,
                Err(e) => {
                    tracing::warn!("workflow feedback sweep: failed to claim candidates: {e}");
                    continue;
                }
            };
            if candidates.is_empty() {
                continue;
            }

            let mut touched_projects = std::collections::HashSet::new();
            let mut incomplete_projects = std::collections::HashSet::new();
            for mut workflow in candidates {
                let Some(pr_number) = workflow.pr_number else {
                    continue;
                };
                let project_key = (workflow.project_id.clone(), workflow.repo.clone());
                if touched_projects.insert(project_key.clone()) {
                    if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                        if let Err(e) = project_store
                            .record_feedback_sweep_started(
                                &workflow.project_id,
                                workflow.repo.as_deref(),
                            )
                            .await
                        {
                            tracing::warn!(
                                project_id = %workflow.project_id,
                                "workflow feedback sweep: failed to mark project sweep start: {e}"
                            );
                        }
                    }
                }

                // Short-circuit if path is healthy (avoids a registry DB query on every tick).
                let project_path = if tokio::fs::metadata(&workflow.project_id).await.is_ok() {
                    warned_unresolvable.remove(&project_key);
                    std::path::PathBuf::from(&workflow.project_id)
                } else if let Some(registry) = state.core.project_registry.as_deref() {
                    match registry.resolve_path(&workflow.project_id).await {
                        Ok(None) | Err(_) => {
                            if warned_unresolvable.insert(project_key.clone()) {
                                tracing::warn!(
                                    project_id = %workflow.project_id,
                                    "sweeper: project path unresolvable, skipping"
                                );
                            }
                            incomplete_projects.insert(project_key.clone());
                            let _ = issue_workflows
                                .release_feedback_claim(
                                    &workflow.project_id,
                                    workflow.repo.as_deref(),
                                    pr_number,
                                    "sweeper: project path unresolvable",
                                )
                                .await;
                            continue;
                        }
                        Ok(Some(resolved))
                            if resolved.as_path() != std::path::Path::new(&workflow.project_id) =>
                        {
                            if let Err(e) = issue_workflows
                                .repair_project_id(&workflow.id, &resolved.to_string_lossy())
                                .await
                            {
                                tracing::error!(
                                    workflow_id = %workflow.id,
                                    error = %e,
                                    "sweeper: failed to repair project_id"
                                );
                                let _ = issue_workflows
                                    .release_feedback_claim(
                                        &workflow.project_id,
                                        workflow.repo.as_deref(),
                                        pr_number,
                                        &format!("sweeper: failed to repair project_id: {e}"),
                                    )
                                    .await;
                                continue;
                            }
                            let new_project_id = resolved.to_string_lossy().into_owned();
                            workflow.id = workflow_id(
                                &new_project_id,
                                workflow.repo.as_deref(),
                                workflow.issue_number,
                            );
                            workflow.project_id = new_project_id;
                            warned_unresolvable.remove(&project_key);
                            resolved
                        }
                        Ok(Some(resolved)) => resolved,
                    }
                } else {
                    std::path::PathBuf::from(&workflow.project_id)
                };

                let req = crate::task_runner::CreateTaskRequest {
                    pr: Some(pr_number),
                    project: Some(project_path),
                    repo: workflow.repo.clone(),
                    source: Some("workflow_feedback".to_string()),
                    ..Default::default()
                };

                match task_routes::enqueue_task_background(state.clone(), req, None).await {
                    Ok(task_id) => {
                        if let Err(e) = issue_workflows
                            .bind_feedback_task_if_claimed(
                                &workflow.project_id,
                                workflow.repo.as_deref(),
                                pr_number,
                                &task_id.0,
                            )
                            .await
                        {
                            incomplete_projects.insert(project_key.clone());
                            tracing::warn!(
                                project_id = %workflow.project_id,
                                pr = pr_number,
                                task_id = %task_id.0,
                                "workflow feedback sweep: failed to bind real task id: {e}"
                            );
                            continue;
                        }
                        tracing::info!(
                            project_id = %workflow.project_id,
                            pr = pr_number,
                            task_id = %task_id.0,
                            "workflow feedback sweep: PR task enqueued"
                        );
                    }
                    Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow {
                        retry_after_secs,
                    }) => {
                        incomplete_projects.insert(project_key.clone());
                        let _ = issue_workflows
                            .release_feedback_claim(
                                &workflow.project_id,
                                workflow.repo.as_deref(),
                                pr_number,
                                &format!(
                                    "feedback sweep deferred by maintenance window; retry after {retry_after_secs}s"
                                ),
                            )
                            .await;
                        if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                            let _ = project_store
                                .record_paused(
                                    &workflow.project_id,
                                    workflow.repo.as_deref(),
                                    &format!(
                                        "feedback sweep paused by maintenance window; retry after {retry_after_secs}s"
                                    ),
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        incomplete_projects.insert(project_key.clone());
                        let _ = issue_workflows
                            .release_feedback_claim(
                                &workflow.project_id,
                                workflow.repo.as_deref(),
                                pr_number,
                                &format!("feedback sweep enqueue failed: {e}"),
                            )
                            .await;
                        tracing::warn!(
                            project_id = %workflow.project_id,
                            pr = pr_number,
                            "workflow feedback sweep: failed to enqueue PR task: {e}"
                        );
                        if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                            let _ = project_store
                                .record_degraded(
                                    &workflow.project_id,
                                    workflow.repo.as_deref(),
                                    &format!(
                                        "feedback sweep enqueue failed for pr:{pr_number}: {e}"
                                    ),
                                )
                                .await;
                        }
                    }
                }
            }

            if let Some(project_store) = state.core.project_workflow_store.as_ref() {
                for (project_id, repo) in touched_projects {
                    if incomplete_projects.contains(&(project_id.clone(), repo.clone())) {
                        continue;
                    }
                    if let Err(e) = project_store
                        .record_feedback_sweep_completed(&project_id, repo.as_deref())
                        .await
                    {
                        tracing::warn!(
                            project_id = %project_id,
                            "workflow feedback sweep: failed to mark project sweep completion: {e}"
                        );
                    }
                }
            }
        }
    });
}

/// Re-dispatch tasks that were recovered to pending after server restart.
/// These had PRs when the server crashed and need their review loop re-started.
/// Without this, recovered tasks silently hang in pending forever.
///
/// Each task is re-dispatched in a background tokio task so that permit
/// acquisition never blocks serve() — if more tasks exist than available
/// concurrency slots, the background futures will simply wait in queue.
pub(super) fn spawn_pr_recovery(state: &Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move {
        let recovered = collect_pr_recovery_tasks(&state).await;
        if !recovered.is_empty() {
            tracing::info!(
                count = recovered.len(),
                "startup: re-dispatching recovered pending task(s) with PRs"
            );
        }
        for task in recovered {
            let state = state.clone();
            tokio::spawn(async move {
                let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "pr").await
                else {
                    return;
                };
                let pr_url = task.pr_url.as_deref().unwrap_or("");
                // Issue 4: robust parsing handles /pull/42/files and #fragment suffixes
                let pr_num = match super::parse_pr_num_from_url(pr_url) {
                    Some(n) => n,
                    None => {
                        // pr_url is present but unparseable (empty, corrupted, or
                        // non-standard).  Simply returning would leave the task stuck in
                        // 'pending' forever — fail-close it instead so operators can see
                        // it and the re-dispatch filter never picks it up again.
                        tracing::error!(
                            task_id = ?task.id,
                            pr_url,
                            "startup recovery: cannot parse PR number from URL — marking task failed"
                        );
                        let bad_url = pr_url.to_owned();
                        if let Err(e) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(format!(
                                    "startup recovery: unparseable pr_url: {bad_url}"
                                ));
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {e}"
                            );
                        }
                        // Fire completion callback so intake sources (e.g. GitHub Issues
                        // poller) remove this task from their `dispatched` map. Without
                        // this the issue stays marked as dispatched forever and will never
                        // be re-queued, causing a silent production deadlock.
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };

                // Issues 2 & 3: resolve canonical project path. Prefer the
                // persisted absolute project_root (set at dispatch time); fall
                // back to registry/repo lookup only when it is absent.
                let project_path = if let Some(root) = task.project_root.clone() {
                    Some(root)
                } else {
                    match task.repo.as_deref() {
                        Some(repo) => {
                            if let Some(registry) = state.core.project_registry.as_deref() {
                                match registry.resolve_path(repo).await {
                                    Ok(Some(p)) => Some(p),
                                    Ok(None) => Some(std::path::PathBuf::from(repo)),
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = ?task.id,
                                            repo,
                                            "startup recovery: registry lookup failed: {e}, using repo as path"
                                        );
                                        Some(std::path::PathBuf::from(repo))
                                    }
                                }
                            } else {
                                Some(std::path::PathBuf::from(repo))
                            }
                        }
                        None => None,
                    }
                };

                let canonical = match task_runner::resolve_canonical_project(project_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to resolve project path: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let project_id = canonical.to_string_lossy().into_owned();

                // Issue 1: acquire permit here inside the spawned future so serve()
                // is never blocked waiting for a concurrency slot.
                let permit = match state
                    .concurrency
                    .task_queue
                    .acquire(&project_id, task.priority)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to acquire concurrency permit: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };

                let req = build_recovered_request(&task, canonical, None, Some(pr_num));
                // Use the three-tier select_agent() so that an explicit agent
                // pin stored in request_settings (Tier 1) and project-level
                // defaults (Tier 2a) are honoured, not bypassed by a raw
                // complexity dispatch.
                let agent = match task_routes::select_agent(
                    &req,
                    &state.core.server.agent_registry,
                    None,
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
                state.core.tasks.register_task_stream(&task.id);
                task_runner::spawn_preregistered_task(
                    task.id,
                    state.core.tasks.clone(),
                    agent,
                    reviewer,
                    Arc::new(state.core.server.config.clone()),
                    state.engines.skills.clone(),
                    state.observability.events.clone(),
                    state.interceptors.clone(),
                    req,
                    state.concurrency.workspace_mgr.clone(),
                    permit,
                    state.intake.completion_callback.clone(),
                    state.core.issue_workflow_store.clone(),
                    state.core.workflow_runtime_store.clone(),
                    state
                        .core
                        .server
                        .config
                        .server
                        .allowed_project_roots
                        .clone(),
                    None,
                )
                .await;
            });
        }
    });
}

fn workflow_state_needs_pr_recovery(state: IssueLifecycleState) -> bool {
    matches!(state, IssueLifecycleState::AddressingFeedback)
}

fn task_is_pr_recovery_candidate(task: &task_runner::TaskState) -> bool {
    matches!(task.status, task_runner::TaskStatus::Pending) && task.pr_url.is_some()
}

fn workflow_recovery_task_ids(
    workflows: &[IssueWorkflowInstance],
) -> std::collections::HashSet<task_runner::TaskId> {
    workflows
        .iter()
        .filter(|workflow| workflow_state_needs_pr_recovery(workflow.state))
        .filter_map(|workflow| {
            workflow
                .active_task_id
                .as_deref()
                .filter(|task_id| !is_feedback_claim_placeholder(task_id))
                .map(|task_id| harness_core::types::TaskId(task_id.to_string()))
        })
        .collect()
}

async fn collect_pr_recovery_tasks(state: &Arc<AppState>) -> Vec<task_runner::TaskState> {
    let tasks = state.core.tasks.list_all();
    let mut recovered = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(workflows) = state.core.issue_workflow_store.as_ref() {
        match workflows.list().await {
            Ok(workflows) => {
                let workflow_task_ids = workflow_recovery_task_ids(&workflows);
                for task in &tasks {
                    if workflow_task_ids.contains(&task.id) && task_is_pr_recovery_candidate(task) {
                        seen.insert(task.id.clone());
                        recovered.push(task.clone());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "startup: failed to list issue workflows for PR recovery, falling back to task-only recovery: {e}"
                );
            }
        }
    }

    for task in tasks {
        if seen.contains(&task.id) {
            continue;
        }
        if task_is_pr_recovery_candidate(&task) {
            recovered.push(task);
        }
    }

    recovered
}

/// Re-dispatch review/planner tasks that were recovered into their kind-specific
/// waiting states after a restart.
pub(super) fn spawn_system_task_recovery(state: &Arc<AppState>) {
    let recovered: Vec<_> = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .filter(|task| {
            matches!(
                task.status,
                task_runner::TaskStatus::ReviewWaiting | task_runner::TaskStatus::PlannerWaiting
            )
        })
        .collect();
    if !recovered.is_empty() {
        tracing::info!(
            count = recovered.len(),
            "startup: re-dispatching recovered review/planner task(s)"
        );
        for task in recovered {
            let state = state.clone();
            tokio::spawn(async move {
                let project_path = task
                    .project_root
                    .clone()
                    .or_else(|| task.repo.as_deref().map(std::path::PathBuf::from));
                let canonical = match task_runner::resolve_canonical_project(project_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to resolve project path: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let project_id = canonical.to_string_lossy().into_owned();

                let queue = match recovery_queue_domain(task.task_kind) {
                    task_routes::QueueDomain::Primary => state.concurrency.task_queue.clone(),
                    task_routes::QueueDomain::Review => state.concurrency.review_task_queue.clone(),
                };
                let permit = match queue.acquire(&project_id, task.priority).await {
                    Ok(p) => p,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to acquire concurrency permit: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };

                let req = build_recovered_request(&task, canonical, None, None);
                if req.prompt.is_none() {
                    let reason = format!(
                        "startup recovery: {} task has no restart-safe input metadata",
                        task.task_kind.as_ref()
                    );
                    tracing::error!(task_id = ?task.id, "{reason}");
                    if let Err(pe) =
                        task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.error = Some(reason);
                        })
                        .await
                    {
                        tracing::error!(
                            task_id = ?task.id,
                            "startup recovery: failed to persist failed status: {pe}; \
                             skipping completion callback to avoid state split"
                        );
                        return;
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }

                let agent = match task_routes::select_agent(
                    &req,
                    &state.core.server.agent_registry,
                    None,
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
                state.core.tasks.register_task_stream(&task.id);
                task_runner::spawn_preregistered_task(
                    task.id,
                    state.core.tasks.clone(),
                    agent,
                    reviewer,
                    Arc::new(state.core.server.config.clone()),
                    state.engines.skills.clone(),
                    state.observability.events.clone(),
                    state.interceptors.clone(),
                    req,
                    state.concurrency.workspace_mgr.clone(),
                    permit,
                    state.intake.completion_callback.clone(),
                    state.core.issue_workflow_store.clone(),
                    state.core.workflow_runtime_store.clone(),
                    state
                        .core
                        .server
                        .config
                        .server
                        .allowed_project_roots
                        .clone(),
                    None,
                )
                .await;
            });
        }
    }
}

/// Re-dispatch tasks recovered from plan/triage checkpoints but without a PR.
/// Source A (spawn_pr_recovery) handles tasks with `pr_url` set. Source B (this
/// function) picks up remaining pending tasks that have a plan or triage
/// checkpoint — these are issue/prompt tasks interrupted during planning that
/// need to continue execution.
/// If the DB query fails, log a warning and skip (startup must not abort).
pub(super) async fn spawn_checkpoint_recovery(state: &Arc<AppState>) {
    let checkpoint_tasks = match state.core.tasks.pending_tasks_with_checkpoint().await {
        Ok(pairs) => pairs,
        Err(e) => {
            tracing::warn!(
                "startup: failed to query checkpoint tasks, \
                     skipping plan/triage redispatch: {e}"
            );
            vec![]
        }
    };
    if !checkpoint_tasks.is_empty() {
        tracing::info!(
            count = checkpoint_tasks.len(),
            "startup: re-dispatching recovered pending task(s) with plan/triage checkpoints"
        );
        for (task, _checkpoint) in checkpoint_tasks {
            let state = state.clone();
            tokio::spawn(async move {
                let Some(task) =
                    await_startup_recovery_ready_task(&state, &task.id, "checkpoint").await
                else {
                    return;
                };
                // Reconstruct request type: parse issue number from the
                // authoritative external_id ("issue:<n>") first, then fall
                // back to the human-readable description for older rows.
                // PR tasks are handled by Source A and never appear here.
                let issue_num = task
                    .external_id
                    .as_deref()
                    .and_then(|eid| eid.strip_prefix("issue:"))
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| {
                        task.description
                            .as_deref()
                            .and_then(|d| d.strip_prefix("issue #"))
                            .and_then(|s| s.split_whitespace().next())
                            .and_then(|s| s.parse::<u64>().ok())
                    });

                if issue_num.is_none() {
                    let reason = if matches!(task.task_kind, task_runner::TaskKind::Prompt) {
                        "prompt task cannot be recovered after restart: original prompt text is not persisted"
                    } else {
                        "checkpoint task has no parseable issue number — skipping"
                    };
                    tracing::warn!(task_id = ?task.id, "{reason}");
                    let mut failed = task.clone();
                    failed.status = task_runner::TaskStatus::Failed;
                    failed
                        .scheduler
                        .mark_terminal(&task_runner::TaskStatus::Failed);
                    failed.error = Some(reason.to_string());
                    state.core.tasks.cache.insert(failed.id.clone(), failed);
                    if let Err(e) = state.core.tasks.persist(&task.id).await {
                        tracing::warn!(
                            task_id = ?task.id,
                            "startup recovery: failed to persist failed status: {e}"
                        );
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }

                let project_path = if let Some(root) = task.project_root.clone() {
                    Some(root)
                } else {
                    match task.repo.as_deref() {
                        Some(repo) => {
                            if let Some(registry) = state.core.project_registry.as_deref() {
                                match registry.resolve_path(repo).await {
                                    Ok(Some(p)) => Some(p),
                                    Ok(None) => Some(std::path::PathBuf::from(repo)),
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = ?task.id,
                                            repo,
                                            "startup recovery: registry lookup failed: \
                                             {e}, using repo as path"
                                        );
                                        Some(std::path::PathBuf::from(repo))
                                    }
                                }
                            } else {
                                Some(std::path::PathBuf::from(repo))
                            }
                        }
                        None => None,
                    }
                };

                let canonical = match task_runner::resolve_canonical_project(project_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to resolve project path: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let project_id = canonical.to_string_lossy().into_owned();
                record_runtime_recovery_requested(
                    &state,
                    &canonical,
                    &task,
                    issue_num,
                    "checkpoint",
                )
                .await;

                let permit = match state
                    .concurrency
                    .task_queue
                    .acquire(&project_id, task.priority)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        let reason =
                            format!("startup recovery: failed to acquire concurrency permit: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };

                let Some(issue) = issue_num else { return };
                let req = build_recovered_request(&task, canonical, Some(issue), None);

                // Use the three-tier select_agent() so that an explicit agent
                // pin stored in request_settings (Tier 1) and project-level
                // defaults (Tier 2a) are honoured, not bypassed by a raw
                // complexity dispatch.
                let agent = match task_routes::select_agent(
                    &req,
                    &state.core.server.agent_registry,
                    None,
                ) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
                let (reviewer, _) = super::resolve_reviewer(
                    &state.core.server.agent_registry,
                    &state.core.server.config.agents.review,
                    agent.name(),
                );
                state.core.tasks.register_task_stream(&task.id);
                task_runner::spawn_preregistered_task(
                    task.id,
                    state.core.tasks.clone(),
                    agent,
                    reviewer,
                    Arc::new(state.core.server.config.clone()),
                    state.engines.skills.clone(),
                    state.observability.events.clone(),
                    state.interceptors.clone(),
                    req,
                    state.concurrency.workspace_mgr.clone(),
                    permit,
                    state.intake.completion_callback.clone(),
                    state.core.issue_workflow_store.clone(),
                    state.core.workflow_runtime_store.clone(),
                    state
                        .core
                        .server
                        .config
                        .server
                        .allowed_project_roots
                        .clone(),
                    None,
                )
                .await;
            });
        }
    }
}

/// Re-dispatch leftover pending tasks that have no PR URL and no checkpoint.
///
/// This recovery runs last so the PR/system/checkpoint-specific startup paths
/// can claim their own rows first. The remaining bucket represents tasks that
/// crashed before their first checkpoint was written.
pub(super) async fn spawn_orphan_pending_recovery(state: &Arc<AppState>) {
    let orphan_tasks = match state.core.tasks.pending_orphan_tasks().await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                "startup: failed to query orphan pending tasks, skipping orphan redispatch: {e}"
            );
            return;
        }
    };
    if orphan_tasks.is_empty() {
        return;
    }

    let mut recoverable = Vec::new();
    let mut failed = Vec::new();
    for task in orphan_tasks {
        let (issue, pr) = parse_issue_pr(&task);
        if issue.is_some() || pr.is_some() || task_allows_prompt_orphan_recovery(&task) {
            recoverable.push((task, issue, pr));
        } else {
            failed.push(task);
        }
    }

    tracing::info!(
        recovered = recoverable.len(),
        failed = failed.len(),
        "startup: orphan pending recovery summary"
    );

    for task in failed {
        let state = state.clone();
        tokio::spawn(async move {
            let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "orphan").await
            else {
                return;
            };
            let reason = orphan_recovery_failure_reason(&task).to_string();
            tracing::warn!(task_id = ?task.id, "{reason}");
            let task_id = task.id.clone();
            if let Err(pe) =
                task_runner::mutate_and_persist(&state.core.tasks, &task_id, move |s| {
                    s.status = task_runner::TaskStatus::Failed;
                    s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                    s.error = Some(reason);
                })
                .await
            {
                tracing::error!(
                    task_id = ?task.id,
                    "startup recovery: failed to persist failed status: {pe}; \
                     skipping completion callback to avoid state split"
                );
                return;
            }
            if let Some(cb) = &state.intake.completion_callback {
                if let Some(final_state) = state.core.tasks.get(&task_id) {
                    cb(final_state).await;
                }
            }
        });
    }

    for (task, issue, pr) in recoverable {
        let state = state.clone();
        tokio::spawn(async move {
            let Some(task) = await_startup_recovery_ready_task(&state, &task.id, "orphan").await
            else {
                return;
            };
            let project_path = if let Some(root) = task.project_root.clone() {
                Some(root)
            } else {
                match task.repo.as_deref() {
                    Some(repo) => {
                        if let Some(registry) = state.core.project_registry.as_deref() {
                            match registry.resolve_path(repo).await {
                                Ok(Some(p)) => Some(p),
                                Ok(None) => Some(std::path::PathBuf::from(repo)),
                                Err(e) => {
                                    tracing::warn!(
                                        task_id = ?task.id,
                                        repo,
                                        "startup recovery: registry lookup failed: \
                                         {e}, using repo as path"
                                    );
                                    Some(std::path::PathBuf::from(repo))
                                }
                            }
                        } else {
                            Some(std::path::PathBuf::from(repo))
                        }
                    }
                    None => None,
                }
            };

            let canonical = match task_runner::resolve_canonical_project(project_path).await {
                Ok(c) => c,
                Err(e) => {
                    let reason = format!("startup recovery: failed to resolve project path: {e}");
                    tracing::error!(task_id = ?task.id, "{reason}");
                    if let Err(pe) =
                        task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                            s.error = Some(reason);
                        })
                        .await
                    {
                        tracing::error!(
                            task_id = ?task.id,
                            "startup recovery: failed to persist failed status: {pe}; \
                             skipping completion callback to avoid state split"
                        );
                        return;
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }
            };
            let project_id = canonical.to_string_lossy().into_owned();
            record_runtime_recovery_requested(&state, &canonical, &task, issue, "orphan").await;

            let queue = match recovery_queue_domain(task.task_kind) {
                task_routes::QueueDomain::Primary => state.concurrency.task_queue.clone(),
                task_routes::QueueDomain::Review => state.concurrency.review_task_queue.clone(),
            };
            let permit = match queue.acquire(&project_id, task.priority).await {
                Ok(p) => p,
                Err(e) => {
                    let reason =
                        format!("startup recovery: failed to acquire concurrency permit: {e}");
                    tracing::error!(task_id = ?task.id, "{reason}");
                    if let Err(pe) =
                        task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                            s.error = Some(reason);
                        })
                        .await
                    {
                        tracing::error!(
                            task_id = ?task.id,
                            "startup recovery: failed to persist failed status: {pe}; \
                             skipping completion callback to avoid state split"
                        );
                        return;
                    }
                    if let Some(cb) = &state.intake.completion_callback {
                        if let Some(final_state) = state.core.tasks.get(&task.id) {
                            cb(final_state).await;
                        }
                    }
                    return;
                }
            };

            let req = build_recovered_request(&task, canonical, issue, pr);
            let agent =
                match task_routes::select_agent(&req, &state.core.server.agent_registry, None) {
                    Ok(a) => a,
                    Err(e) => {
                        let reason = format!("startup recovery: failed to select agent: {e}");
                        tracing::error!(task_id = ?task.id, "{reason}");
                        if let Err(pe) =
                            task_runner::mutate_and_persist(&state.core.tasks, &task.id, move |s| {
                                s.status = task_runner::TaskStatus::Failed;
                                s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                                s.error = Some(reason);
                            })
                            .await
                        {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to persist failed status: {pe}; \
                                 skipping completion callback to avoid state split"
                            );
                            return;
                        }
                        if let Some(cb) = &state.intake.completion_callback {
                            if let Some(final_state) = state.core.tasks.get(&task.id) {
                                cb(final_state).await;
                            }
                        }
                        return;
                    }
                };
            let (reviewer, _) = super::resolve_reviewer(
                &state.core.server.agent_registry,
                &state.core.server.config.agents.review,
                agent.name(),
            );
            state.core.tasks.register_task_stream(&task.id);
            task_runner::spawn_preregistered_task(
                task.id,
                state.core.tasks.clone(),
                agent,
                reviewer,
                Arc::new(state.core.server.config.clone()),
                state.engines.skills.clone(),
                state.observability.events.clone(),
                state.interceptors.clone(),
                req,
                state.concurrency.workspace_mgr.clone(),
                permit,
                state.intake.completion_callback.clone(),
                state.core.issue_workflow_store.clone(),
                state.core.workflow_runtime_store.clone(),
                state
                    .core
                    .server
                    .config
                    .server
                    .allowed_project_roots
                    .clone(),
                None,
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECONCILE_GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    #[test]
    fn runtime_dispatch_profile_selector_applies_workflow_overrides() {
        let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
            runtime_kind: "claude_code".to_string(),
            runtime_profile: "claude-default".to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            timeout_secs: Some(600),
            ..Default::default()
        };
        policy.workflow_profiles.insert(
            "github_issue_pr".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_kind: Some("codex_exec".to_string()),
                runtime_profile: Some("codex-high".to_string()),
                model: Some("gpt-5.4".to_string()),
                timeout_secs: Some(1200),
                ..Default::default()
            },
        );
        policy.workflow_profiles.insert(
            "repo_backlog".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_kind: None,
                runtime_profile: Some("codex-backlog".to_string()),
                ..Default::default()
            },
        );
        policy.activity_profiles.insert(
            "replan_issue".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_kind: Some("codex_jsonrpc".to_string()),
                runtime_profile: Some("codex-replan".to_string()),
                model: Some("gpt-5.4-mini".to_string()),
                ..Default::default()
            },
        );
        policy.workflow_activity_profiles.insert(
            "github_issue_pr".to_string(),
            std::collections::BTreeMap::from([(
                "replan_issue".to_string(),
                harness_core::config::workflow::RuntimeDispatchProfileOverride {
                    runtime_profile: Some("codex-issue-replan".to_string()),
                    model: Some("gpt-5.5".to_string()),
                    timeout_secs: Some(900),
                    ..Default::default()
                },
            )]),
        );

        let selector = runtime_dispatch_profile_selector(&policy);
        let issue_profile = selector.select_for_workflow(Some("github_issue_pr"));
        assert_eq!(issue_profile.kind, RuntimeKind::CodexExec);
        assert_eq!(issue_profile.name, "codex-high");
        assert_eq!(issue_profile.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(issue_profile.timeout_secs, Some(1200));
        let replan_profile = selector.select(Some("github_issue_pr"), Some("replan_issue"));
        assert_eq!(replan_profile.kind, RuntimeKind::CodexJsonrpc);
        assert_eq!(replan_profile.name, "codex-issue-replan");
        assert_eq!(replan_profile.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(replan_profile.timeout_secs, Some(900));
        let global_replan_profile = selector.select(Some("repo_backlog"), Some("replan_issue"));
        assert_eq!(global_replan_profile.kind, RuntimeKind::CodexJsonrpc);
        assert_eq!(global_replan_profile.name, "codex-replan");
        assert_eq!(global_replan_profile.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(global_replan_profile.timeout_secs, Some(600));
        let backlog_profile = selector.select_for_workflow(Some("repo_backlog"));
        assert_eq!(backlog_profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(backlog_profile.name, "codex-backlog");
        assert_eq!(backlog_profile.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(backlog_profile.timeout_secs, Some(600));
        let default_profile = selector.select_for_workflow(Some("quality_gate"));
        assert_eq!(default_profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(default_profile.name, "claude-default");
        assert_eq!(default_profile.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(default_profile.timeout_secs, Some(600));
    }

    #[test]
    fn runtime_dispatch_profile_selector_drops_codex_approval_for_non_codex_override() {
        let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
            runtime_kind: "codex_jsonrpc".to_string(),
            runtime_profile: "codex-default".to_string(),
            approval_policy: Some("on-request".to_string()),
            ..Default::default()
        };
        policy.workflow_profiles.insert(
            "claude_review".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_kind: Some("claude_code".to_string()),
                runtime_profile: Some("claude-review".to_string()),
                approval_policy: Some("never".to_string()),
                ..Default::default()
            },
        );
        policy.workflow_profiles.insert(
            "codex_review".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_profile: Some("codex-review".to_string()),
                ..Default::default()
            },
        );

        let selector = runtime_dispatch_profile_selector(&policy);
        let claude_profile = selector.select_for_workflow(Some("claude_review"));
        assert_eq!(claude_profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(claude_profile.name, "claude-review");
        assert_eq!(claude_profile.approval_policy, None);

        let codex_profile = selector.select_for_workflow(Some("codex_review"));
        assert_eq!(codex_profile.kind, RuntimeKind::CodexJsonrpc);
        assert_eq!(codex_profile.name, "codex-review");
        assert_eq!(codex_profile.approval_policy.as_deref(), Some("on-request"));
    }

    #[test]
    fn runtime_dispatch_profile_drops_codex_approval_for_non_codex_default() {
        let policy = harness_core::config::workflow::RuntimeDispatchPolicy {
            runtime_kind: "claude_code".to_string(),
            runtime_profile: "claude-default".to_string(),
            approval_policy: Some("on-request".to_string()),
            ..Default::default()
        };

        let profile = runtime_dispatch_profile(&policy);
        assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
        assert_eq!(profile.name, "claude-default");
        assert_eq!(profile.approval_policy, None);
    }

    #[test]
    fn runtime_worker_loop_policy_keeps_polling_when_server_root_disables_worker() {
        let policy = harness_core::config::workflow::RuntimeWorkerPolicy {
            enabled: false,
            concurrency: 8,
            ..Default::default()
        };

        let effective = runtime_worker_loop_policy(policy);

        assert!(effective.enabled);
        assert_eq!(effective.concurrency, 1);
        assert_eq!(effective.interval_secs, 5);
    }

    // ARCH-GH-EXEMPT test double: mirrors the logic of fetch_pr_state_by_url in
    // reconciliation.rs, but with an injectable gh_bin so tests run without a live
    // GitHub connection. Keep the parsing (trim_matches('"'), to_uppercase) in sync
    // with classify_pr_output in reconciliation.rs to avoid logic drift.
    /// Fetch the current GitHub state for a PR identified by `pr_url`.
    ///
    /// Returns `Some((raw_state, new_status))` only for actionable terminal states
    /// (MERGED → Done, CLOSED → Cancelled). Any transient failure (I/O error,
    /// non-zero exit, timeout, unexpected state) returns `None` so the caller
    /// skips the task silently.
    async fn fetch_pr_github_state(
        gh_bin: &str,
        task_id: &task_runner::TaskId,
        pr_url: &str,
    ) -> Option<(String, task_runner::TaskStatus)> {
        let output = match tokio::time::timeout(
            RECONCILE_GH_TIMEOUT,
            tokio::process::Command::new(gh_bin)
                .args(["pr", "view", pr_url, "--json", "state", "--jq", ".state"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                tracing::warn!(task_id = %task_id, error = %e, "reconciliation: gh command failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(task_id = %task_id, "reconciliation: gh command timed out");
                return None;
            }
        };
        if !output.status.success() {
            tracing::debug!(
                task_id = %task_id,
                stderr = %String::from_utf8_lossy(&output.stderr),
                "reconciliation: gh pr view returned non-zero"
            );
            return None;
        }
        let raw = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_matches('"')
            .to_uppercase();
        match raw.as_str() {
            "MERGED" => Some((raw.to_string(), task_runner::TaskStatus::Done)),
            "CLOSED" => Some((raw.to_string(), task_runner::TaskStatus::Cancelled)),
            _ => None,
        }
    }

    #[test]
    fn workflow_recovery_task_ids_only_uses_active_addressing_feedback_rows() {
        let mut addressing = IssueWorkflowInstance::new(
            "/tmp/project".to_string(),
            Some("owner/repo".to_string()),
            1,
        );
        addressing.state = IssueLifecycleState::AddressingFeedback;
        addressing.active_task_id = Some("task-1".to_string());

        let mut waiting = IssueWorkflowInstance::new(
            "/tmp/project".to_string(),
            Some("owner/repo".to_string()),
            2,
        );
        waiting.state = IssueLifecycleState::AwaitingFeedback;
        waiting.active_task_id = Some("task-2".to_string());

        let mut no_task = IssueWorkflowInstance::new(
            "/tmp/project".to_string(),
            Some("owner/repo".to_string()),
            3,
        );
        no_task.state = IssueLifecycleState::AddressingFeedback;

        let ids = workflow_recovery_task_ids(&[addressing, waiting, no_task]);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&harness_core::types::TaskId("task-1".to_string())));
    }

    #[test]
    fn workflow_recovery_task_ids_ignores_claim_placeholder_ids() {
        let mut claimed = IssueWorkflowInstance::new(
            "/tmp/project".to_string(),
            Some("owner/repo".to_string()),
            4,
        );
        claimed.state = IssueLifecycleState::AddressingFeedback;
        claimed.active_task_id = Some("claim:workflow-4".to_string());

        let ids = workflow_recovery_task_ids(&[claimed]);
        assert!(ids.is_empty());
    }

    #[test]
    fn parse_issue_pr_uses_persisted_request_identifiers() {
        let issue_settings =
            task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
                issue: Some(944),
                prompt: Some("extra context".to_string()),
                external_id: Some("custom-issue-944".to_string()),
                ..task_runner::CreateTaskRequest::default()
            });
        let mut issue_task = task_runner::TaskState::new(task_runner::TaskId::new());
        issue_task.task_kind = task_runner::TaskKind::Issue;
        issue_task.external_id = Some("custom-issue-944".to_string());
        issue_task.request_settings = Some(issue_settings);
        assert_eq!(parse_issue_pr(&issue_task), (Some(944), None));

        let pr_settings =
            task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
                pr: Some(1040),
                external_id: Some("custom-pr-1040".to_string()),
                ..task_runner::CreateTaskRequest::default()
            });
        let mut pr_task = task_runner::TaskState::new(task_runner::TaskId::new());
        pr_task.task_kind = task_runner::TaskKind::Pr;
        pr_task.external_id = Some("custom-pr-1040".to_string());
        pr_task.request_settings = Some(pr_settings);
        assert_eq!(parse_issue_pr(&pr_task), (None, Some(1040)));
    }

    #[test]
    fn prompt_orphan_recovery_requires_prompt_task_kind() {
        let settings =
            task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
                issue: Some(944),
                prompt: Some("extra context".to_string()),
                ..task_runner::CreateTaskRequest::default()
            });
        let mut issue_task = task_runner::TaskState::new(task_runner::TaskId::new());
        issue_task.task_kind = task_runner::TaskKind::Issue;
        issue_task.request_settings = Some(settings);
        assert!(!task_allows_prompt_orphan_recovery(&issue_task));

        let prompt_settings =
            task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
                prompt: Some("restart-safe prompt".to_string()),
                ..task_runner::CreateTaskRequest::default()
            });
        let mut prompt_task = task_runner::TaskState::new(task_runner::TaskId::new());
        prompt_task.task_kind = task_runner::TaskKind::Prompt;
        prompt_task.request_settings = Some(prompt_settings);
        assert!(task_allows_prompt_orphan_recovery(&prompt_task));
    }

    #[test]
    fn task_is_pr_recovery_candidate_requires_pending_with_pr_url() {
        let mut pending_with_pr = task_runner::TaskState::new(task_runner::TaskId::new());
        pending_with_pr.status = task_runner::TaskStatus::Pending;
        pending_with_pr.pr_url = Some("https://github.com/owner/repo/pull/1".to_string());
        assert!(task_is_pr_recovery_candidate(&pending_with_pr));

        let mut waiting_with_pr = pending_with_pr.clone();
        waiting_with_pr.status = task_runner::TaskStatus::Waiting;
        assert!(!task_is_pr_recovery_candidate(&waiting_with_pr));

        let mut pending_without_pr = pending_with_pr;
        pending_without_pr.pr_url = None;
        assert!(!task_is_pr_recovery_candidate(&pending_without_pr));
    }

    #[test]
    fn warn_dedup_insert_returns_true_first_time_only() {
        let mut warned: std::collections::HashSet<(String, Option<String>)> =
            std::collections::HashSet::new();
        let key = ("/dead/path".to_string(), Some("owner/repo".to_string()));
        assert!(
            warned.insert(key.clone()),
            "first insert should return true (first tick should warn)"
        );
        assert!(
            !warned.insert(key.clone()),
            "second insert should return false (dedup suppresses warn)"
        );
        warned.remove(&key);
        assert!(
            warned.insert(key.clone()),
            "after removal (path recovered), insert returns true again"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_merged_transitions_to_done() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'MERGED\\n'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let task_id = task_runner::TaskId::new();
        let result = fetch_pr_github_state(
            script.to_str().unwrap(),
            &task_id,
            "https://github.com/owner/repo/pull/1",
        )
        .await;
        assert!(
            matches!(result, Some((ref s, task_runner::TaskStatus::Done)) if s == "MERGED"),
            "expected Some((\"MERGED\", Done)), got {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_closed_transitions_to_cancelled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'CLOSED\\n'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let task_id = task_runner::TaskId::new();
        let result = fetch_pr_github_state(
            script.to_str().unwrap(),
            &task_id,
            "https://github.com/owner/repo/pull/1",
        )
        .await;
        assert!(
            matches!(result, Some((ref s, task_runner::TaskStatus::Cancelled)) if s == "CLOSED"),
            "expected Some((\"CLOSED\", Cancelled)), got {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_open_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'OPEN\\n'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let task_id = task_runner::TaskId::new();
        let result = fetch_pr_github_state(
            script.to_str().unwrap(),
            &task_id,
            "https://github.com/owner/repo/pull/1",
        )
        .await;
        assert!(
            result.is_none(),
            "expected None for OPEN state, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_gh_failure_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let task_id = task_runner::TaskId::new();
        let result = fetch_pr_github_state(
            script.to_str().unwrap(),
            &task_id,
            "https://github.com/owner/repo/pull/1",
        )
        .await;
        assert!(
            result.is_none(),
            "expected None for non-zero exit, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_gh_timeout_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let task_id = task_runner::TaskId::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            fetch_pr_github_state(
                script.to_str().unwrap(),
                &task_id,
                "https://github.com/owner/repo/pull/1",
            ),
        )
        .await;
        // The inner function must time out after RECONCILE_GH_TIMEOUT (10s) and return None;
        // the outer 15s guard is a safety net — if it fires, the inner timeout logic is broken.
        match result {
            Ok(inner) => assert!(inner.is_none(), "expected None on timeout, got {inner:?}"),
            Err(_) => panic!("outer test timeout fired before inner function timeout logic"),
        }
    }
}
