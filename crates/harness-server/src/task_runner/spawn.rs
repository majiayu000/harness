use harness_core::agent::CodeAgent;
use harness_core::types::{Decision, Event, SessionId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::request::{
    detect_main_worktree, summarize_request_description, CreateTaskRequest,
    PersistedRequestSettings,
};
use super::state::{RoundResult, TaskState};
use super::store::{mutate_and_persist, TaskStore};
use super::types::{TaskFailureKind, TaskId, TaskKind, TaskPhase, TaskStatus};
use super::CompletionCallback;

/// Patterns that indicate a transient (retryable) failure rather than a permanent one.
const TRANSIENT_PATTERNS: &[&str] = &[
    "at capacity",
    "rate limit",
    "rate_limit",
    "hit your limit",
    "429",
    "502 Bad Gateway",
    "503 Service",
    "overloaded",
    "connection reset",
    "connection refused",
    "broken pipe",
    "EOF",
    "stream idle timeout",
    "stream stall",
    "ECONNRESET",
    "ETIMEDOUT",
    // SQLite transient contention — SQLITE_BUSY / SQLITE_LOCKED
    "database is locked",
    "database table is locked",
    "SQLITE_BUSY",
    "SQLITE_LOCKED",
];

/// Pattern indicating the CLI account-level usage limit has been reached.
/// When detected, the global rate-limit circuit breaker is activated to
/// prevent all other tasks from wasting turns.
const ACCOUNT_LIMIT_PATTERN: &str = "hit your limit";

/// Maximum number of automatic retries for transient failures.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Return `true` when a free-text prompt is complex enough to require a Plan phase
/// before implementation.
///
/// Heuristic: prompt longer than 200 words OR contains 3 or more file-path-like
/// tokens (sequences containing `/` or ending in a recognised source extension).
pub fn prompt_requires_plan(prompt: &str) -> bool {
    let word_count = prompt.split_whitespace().count();
    if word_count > 200 {
        return true;
    }
    let file_path_count = prompt
        .split_whitespace()
        .filter(|tok| {
            // Exclude XML/HTML tags — `</foo>` contains '/' but is not a file path.
            // System prompts wrap user data in `<external_data>...</external_data>`
            // which would otherwise trigger false positives.
            let is_xml_tag = tok.starts_with('<');
            if is_xml_tag {
                return false;
            }
            tok.contains('/') || {
                let lower = tok.to_lowercase();
                lower.ends_with(".rs")
                    || lower.ends_with(".ts")
                    || lower.ends_with(".tsx")
                    || lower.ends_with(".go")
                    || lower.ends_with(".py")
                    || lower.ends_with(".toml")
                    || lower.ends_with(".json")
            }
        })
        .count();
    file_path_count >= 3
}

fn classify_task_kind(req: &CreateTaskRequest) -> TaskKind {
    req.task_kind()
}

fn refresh_preregistered_task_metadata(
    entry: &mut TaskState,
    req: &CreateTaskRequest,
    project_root: PathBuf,
    description: Option<String>,
) {
    entry.task_kind = classify_task_kind(req);
    entry.source = req.source.clone();
    entry.external_id = req.external_id.clone();
    entry.repo = req.repo.clone();
    entry.parent_id = req.parent_task_id.clone();
    entry.depends_on = req.depends_on.clone();
    entry.project_root = Some(project_root);
    entry.issue = req.issue;
    entry.description = description;
}

#[cfg(test)]
pub(super) fn is_non_decomposable_prompt_source(source: Option<&str>) -> bool {
    TaskKind::classify(source, None, None).is_non_decomposable_prompt()
}

/// Check if an error message indicates a transient failure that may succeed on retry.
pub(super) fn is_transient_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    TRANSIENT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

/// Extract `(owner, repo, number)` from a GitHub PR URL.
///
/// Expects format: `https://github.com/{owner}/{repo}/pull/{number}[#...]`
pub(super) fn parse_pr_url(pr_url: &str) -> Option<(String, String, u64)> {
    // Strip fragment (e.g. #discussion_r...)
    let url = pr_url.split('#').next().unwrap_or(pr_url);
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    // Expected: ["https:", "", "github.com", owner, repo, "pull", number]
    let pull_idx = parts.iter().rposition(|&p| p == "pull")?;
    if pull_idx + 1 >= parts.len() || pull_idx < 2 {
        return None;
    }
    let number: u64 = parts[pull_idx + 1].parse().ok()?;
    let repo = parts[pull_idx - 1].to_string();
    let owner = parts[pull_idx - 2].to_string();
    Some((owner, repo, number))
}

/// Convert a user-facing timeout value to a Duration.
/// `0` means "no timeout" and maps to ~277 hours (effectively unlimited).
pub fn effective_turn_timeout(secs: u64) -> tokio::time::Duration {
    if secs == 0 {
        tokio::time::Duration::from_secs(999_999)
    } else {
        tokio::time::Duration::from_secs(secs)
    }
}

fn describe_detect_main_worktree_join_error(join_err: &tokio::task::JoinError) -> String {
    if join_err.is_panic() {
        format!("detect_main_worktree panicked: {join_err}")
    } else if join_err.is_cancelled() {
        format!("detect_main_worktree was cancelled: {join_err}")
    } else {
        format!("detect_main_worktree failed: {join_err}")
    }
}

async fn resolve_project_root_with(
    requested_project: Option<PathBuf>,
    detect_worktree: impl FnOnce() -> PathBuf + Send + 'static,
) -> anyhow::Result<PathBuf> {
    match requested_project {
        Some(project) => Ok(project),
        None => tokio::task::spawn_blocking(detect_worktree)
            .await
            .map_err(|join_err| {
                let reason = describe_detect_main_worktree_join_error(&join_err);
                tracing::error!("{reason}");
                anyhow::anyhow!("{reason}")
            }),
    }
}

/// Resolve and canonicalize the project root so the caller can obtain a stable
/// semaphore key before acquiring the concurrency permit.
///
/// When `project` is `None` the main git worktree is detected automatically.
/// Symlinks, relative paths and the `None` sentinel all converge to the same
/// canonical `PathBuf`, preventing the same repository from landing in
/// different per-project buckets due to path aliasing.
///
/// This function only resolves symlinks — it does NOT enforce the HOME-boundary
/// restriction applied by `validate_project_root`. Full validation still
/// happens inside `spawn_task` once the task is running.
pub async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = resolve_project_root_with(project, detect_main_worktree).await?;
    // Best-effort canonicalize: if the path doesn't exist yet (e.g. in tests
    // using a path that will be created later) fall back to the raw path so
    // we at least get a consistent string key.
    Ok(raw.canonicalize().unwrap_or(raw))
}

async fn log_task_failure_event(
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event).await {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

/// Record a task failure, marking the task as failed.
async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.failure_kind = Some(TaskFailureKind::Task);
        s.error = Some(reason);
        s.scheduler.mark_terminal(&TaskStatus::Failed);
    })
    .await
    {
        tracing::error!("failed to persist task failure for {task_id:?}: {e}");
    }
}

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
/// Called before the new task is inserted into the store.
fn detect_cycle(store: &TaskStore, new_id: &TaskId, depends_on: &[TaskId]) -> bool {
    let mut stack: Vec<TaskId> = depends_on.to_vec();
    let mut visited: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    while let Some(dep_id) = stack.pop() {
        if dep_id == *new_id {
            return true;
        }
        if visited.contains(&dep_id) {
            continue;
        }
        visited.insert(dep_id.clone());
        if let Some(dep_state) = store.get(&dep_id) {
            for transitive in &dep_state.depends_on {
                stack.push(transitive.clone());
            }
        }
    }
    false
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
) -> TaskId {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors,
        req,
        detect_main_worktree,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        None,
        None,
    )
    .await
}

/// Register a task with Pending status and return its ID immediately, without waiting
/// for a concurrency permit. Pair with `spawn_preregistered_task` (called from a
/// background tokio task after `task_queue.acquire()`) to begin execution.
pub async fn register_pending_task(store: Arc<TaskStore>, req: &CreateTaskRequest) -> TaskId {
    let task_id = TaskId::new();
    let mut state = TaskState::new(task_id.clone());
    state.task_kind = classify_task_kind(req);
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.parent_id = req.parent_task_id.clone();
    state.depends_on = req.depends_on.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.phase = state.task_kind.default_phase();
    state.description = summarize_request_description(req);
    state.request_settings = Some(PersistedRequestSettings::from_req(req));
    store.insert(&state).await;
    // Register stream channel now so SSE clients can subscribe before execution begins.
    store.register_task_stream(&task_id);
    task_id
}

/// Begin execution for a task pre-registered via `register_pending_task`.
/// The caller must have already acquired a concurrency permit; it is held for the task's lifetime.
/// `group_permit` is an optional semaphore permit that serialises tasks within a conflict group;
/// it is held inside the innermost spawned future for the full duration of task execution.
pub async fn spawn_preregistered_task(
    task_id: TaskId,
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors,
        req,
        detect_main_worktree,
        workspace_mgr,
        permit,
        completion_callback,
        issue_workflow_store,
        Some(task_id),
        group_permit,
    )
    .await;
}

pub(super) async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    mut req: CreateTaskRequest,
    detect_worktree: F,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    preregistered_id: Option<TaskId>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let task_id = if let Some(id) = preregistered_id {
        // Task was pre-registered (e.g. by register_pending_task for batch submission);
        // store.insert and register_task_stream were already called.
        id
    } else {
        let task_id = TaskId::new();
        let mut state = TaskState::new(task_id.clone());
        state.task_kind = classify_task_kind(&req);
        state.source = req.source.clone();
        state.external_id = req.external_id.clone();
        state.repo = req.repo.clone();
        state.priority = req.priority;
        state.phase = state.task_kind.default_phase();
        state.request_settings = Some(PersistedRequestSettings::from_req(&req));
        store.insert(&state).await;
        // Register stream channel before spawning so SSE clients can subscribe immediately.
        store.register_task_stream(&task_id);
        task_id
    };

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);
    // Clones used to store the abort handle after the main future is spawned
    // (store and id are moved into the spawn closure).
    let store_for_abort = store.clone();
    let id_for_abort = id.clone();

    let handle = tokio::spawn(async move {
        // Hold both permits for the task's lifetime so that the group serialisation
        // semaphore is not released until actual execution completes (not just until
        // spawn_preregistered_task returns, which happens almost immediately).
        let _permit = permit;
        let _group_permit = group_permit;
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let home_dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| raw_project.clone());
        let project_root = crate::handlers::validate_project_root(&raw_project, &home_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if req.repo.is_none() {
            req.repo = crate::task_executor::pr_detection::detect_repo_slug(&project_root).await;
        }
        let description = summarize_request_description(&req);
        if let Some(mut entry) = store.cache.get_mut(&id) {
            refresh_preregistered_task_metadata(
                &mut entry,
                &req,
                project_root.clone(),
                description.clone(),
            );
        }
        mutate_and_persist(&store, &id, |s| {
            s.source = req.source.clone();
            s.external_id = req.external_id.clone();
            s.repo = req.repo.clone();
            s.parent_id = req.parent_task_id.clone();
            s.depends_on = req.depends_on.clone();
            s.project_root = Some(project_root.clone());
            s.issue = req.issue;
            s.description = description.clone();
            s.run_generation = s.run_generation.saturating_add(1);
            s.failure_kind = None;
            s.workspace_path = None;
            s.workspace_owner = None;
            s.error = None;
        })
        .await?;
        let run_generation = store
            .get(&id)
            .map(|state| state.run_generation)
            .unwrap_or(1);

        // Parallel dispatch for Complex+ prompt-only tasks when workspace isolation is active.
        // Issue and PR tasks have their own structured prompt flow and are not decomposed.
        let classification = crate::complexity_router::classify(
            req.prompt.as_deref().unwrap_or_default(),
            req.issue,
            req.pr,
        );
        let is_complex = matches!(
            classification.complexity,
            harness_core::agent::TaskComplexity::Complex
                | harness_core::agent::TaskComplexity::Critical
        );
        let task_kind = classify_task_kind(&req);
        if req.issue.is_none()
            && req.pr.is_none()
            && is_complex
            && !task_kind.is_non_decomposable_prompt()
        {
            if let Some(ref wmgr) = workspace_mgr {
                let mut subtask_specs = match crate::parallel_dispatch::decompose(
                    req.prompt.as_deref().unwrap_or_default(),
                ) {
                    Ok(specs) => specs,
                    Err(msg) => {
                        tracing::warn!(task_id = %id.0, "parallel_dispatch rejected: {}", msg);
                        mutate_and_persist(&store, &id, |s| {
                            s.status = TaskStatus::Failed;
                            s.scheduler.mark_terminal(&TaskStatus::Failed);
                            s.error = Some(msg);
                        })
                        .await?;
                        return Ok(());
                    }
                };
                if subtask_specs.len() > 1 {
                    // Prepend sibling-awareness context to each subtask prompt so parallel
                    // agents know what other top-level tasks are running on the same project.
                    let siblings = store.list_siblings(&project_root, &id);
                    if !siblings.is_empty() {
                        let sibling_tasks: Vec<harness_core::prompts::SiblingTask> = siblings
                            .into_iter()
                            .filter_map(|s| {
                                s.description.and_then(|description| {
                                    if description.is_empty() {
                                        None
                                    } else {
                                        Some(harness_core::prompts::SiblingTask {
                                            issue: s.issue,
                                            description,
                                        })
                                    }
                                })
                            })
                            .collect();
                        let ctx = harness_core::prompts::sibling_task_context(&sibling_tasks);
                        subtask_specs = subtask_specs
                            .into_iter()
                            .map(|mut spec| {
                                spec.prompt = format!("{ctx}\n\n{}", spec.prompt);
                                spec
                            })
                            .collect();
                    }
                    let project_config = harness_core::config::project::load_project_config(
                        &project_root,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "failed to load project config for {}: {e}",
                            project_root.display()
                        )
                    })?;
                    let remote = project_config.git.remote.clone();
                    let base_branch = project_config.git.base_branch.clone();

                    // Register subtask IDs in the parent before dispatch.
                    let sub_ids: Vec<TaskId> = (0..subtask_specs.len())
                        .map(|i| harness_core::types::TaskId(format!("{}-p{i}", id.0)))
                        .collect();
                    mutate_and_persist(&store, &id, |s| {
                        s.subtask_ids = sub_ids.clone();
                    })
                    .await?;

                    let context_items = crate::task_executor::helpers::collect_context_items(
                        &skills,
                        &project_root,
                        req.prompt.as_deref().unwrap_or_default(),
                    )
                    .await;

                    let turn_timeout = effective_turn_timeout(req.turn_timeout_secs);
                    let run_result = crate::parallel_dispatch::run_parallel_subtasks(
                        &id,
                        agent.clone(),
                        subtask_specs,
                        wmgr.clone(),
                        &project_root,
                        &remote,
                        &base_branch,
                        context_items,
                        turn_timeout,
                    )
                    .await;

                    // Both sequential and parallel tasks require ALL subtasks to succeed.
                    // With up to MAX_PARALLEL (8) chunks, any() would mark Done on 1/8
                    // success, silently dropping failed work.
                    let succeeded = run_result.results.iter().all(|r| r.response.is_some());
                    mutate_and_persist(&store, &id, |s| {
                        for r in &run_result.results {
                            let detail = if let Some(ref resp) = r.response {
                                if resp.output.is_empty() {
                                    None
                                } else {
                                    Some(resp.output.clone())
                                }
                            } else {
                                r.error.clone()
                            };
                            s.rounds.push(RoundResult {
                                turn: (r.index as u32).saturating_add(1),
                                action: format!("parallel_subtask_{}", r.index),
                                result: if r.response.is_some() {
                                    "success".into()
                                } else {
                                    "failed".into()
                                },
                                detail,
                                first_token_latency_ms: None,
                            });
                        }
                        if succeeded {
                            s.status = TaskStatus::Done;
                            s.scheduler.mark_terminal(&TaskStatus::Done);
                        } else {
                            let failed_count = run_result
                                .results
                                .iter()
                                .filter(|r| r.response.is_none())
                                .count();
                            let total_count = run_result.results.len();
                            s.status = TaskStatus::Failed;
                            s.scheduler.mark_terminal(&TaskStatus::Failed);
                            s.error = Some(if run_result.is_sequential {
                                format!(
                                    "{}/{} sequential subtasks failed; remaining steps were skipped",
                                    failed_count, total_count
                                )
                            } else {
                                format!(
                                    "{}/{} parallel subtasks failed",
                                    failed_count, total_count
                                )
                            });
                        }
                    })
                    .await?;
                    return Ok(());
                }
            }
        }

        // Planning gate: complex prompt-only tasks must go through the Plan phase
        // before implementation to reduce drift.
        // Heuristic: prompt longer than 200 words OR containing 3+ file path tokens.
        if req.issue.is_none() && req.pr.is_none() && !task_kind.is_non_decomposable_prompt() {
            if let Some(ref prompt) = req.prompt {
                if prompt_requires_plan(prompt) {
                    mutate_and_persist(&store, &id, |s| {
                        if s.phase == TaskPhase::Implement {
                            s.phase = TaskPhase::Plan;
                        }
                    })
                    .await?;
                    tracing::info!(
                        task_id = %id.0,
                        "planning gate: complex prompt — forcing Plan phase before Implement"
                    );
                }
            }
        }

        // If workspace isolation is configured, create a per-task git worktree.
        let run_project = if let Some(ref wmgr) = workspace_mgr {
            let project_config = harness_core::config::project::load_project_config(&project_root)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to load project config for {}: {e}",
                        project_root.display()
                    )
                })?;
            let lease = match wmgr
                .create_workspace(
                    &id,
                    &project_root,
                    &project_config.git.remote,
                    &project_config.git.base_branch,
                    run_generation,
                )
                .await
            {
                Ok(lease) => lease,
                Err(err) => {
                    let error_message = err.to_string();
                    let workspace_path = err.workspace_path().to_path_buf();
                    let workspace_owner =
                        err.workspace_owner().map(std::string::ToString::to_string);
                    mutate_and_persist(&store, &id, |s| {
                        s.status = TaskStatus::Failed;
                        s.failure_kind = Some(TaskFailureKind::WorkspaceLifecycle);
                        s.error = Some(error_message.clone());
                        s.workspace_path = Some(workspace_path.clone());
                        s.workspace_owner = workspace_owner.clone();
                        s.rounds.push(RoundResult {
                            turn: s.turn.saturating_add(1),
                            action: "workspace_admission".into(),
                            result: "workspace_lifecycle_failure".into(),
                            detail: Some(error_message.clone()),
                            first_token_latency_ms: None,
                        });
                    })
                    .await?;
                    tracing::warn!(task_id = %id.0, error = %error_message, "workspace admission failed");
                    return Ok(());
                }
            };
            tracing::info!(
                task_id = %id.0,
                workspace_path = %lease.workspace_path.display(),
                workspace_owner = %lease.owner_session,
                run_generation = lease.run_generation,
                workspace_decision = ?lease.decision,
                "workspace admitted"
            );
            mutate_and_persist(&store, &id, |s| {
                s.workspace_path = Some(lease.workspace_path.clone());
                s.workspace_owner = Some(lease.owner_session.clone());
                s.run_generation = lease.run_generation;
            })
            .await?;
            lease.workspace_path
        } else {
            project_root
        };

        // Retry loop: on transient errors, back off and retry up to MAX_TRANSIENT_RETRIES.
        // Retrying inside the spawn means the stream stays open and the task actually re-runs.
        let mut transient_attempts = 0u32;
        // Track total turns used across all transient-retry attempts so the
        // max_turns budget is enforced globally over the full task lifecycle.
        let mut total_turns_used: u32 = 0;
        let task_result = loop {
            // Wait if global rate-limit circuit breaker is active (another task hit the limit).
            store.wait_for_rate_limit().await;

            let result = crate::task_executor::run_task(
                &store,
                &id,
                agent.as_ref(),
                reviewer.as_deref(),
                skills.clone(),
                events.clone(),
                interceptors.clone(),
                &req,
                run_project.clone(),
                server_config.as_ref(),
                issue_workflow_store.clone(),
                &mut total_turns_used,
            )
            .await;

            match result {
                ok @ Ok(()) => break ok,
                Err(ref e)
                    if is_transient_error(&format!("{:#}", e))
                        && transient_attempts < MAX_TRANSIENT_RETRIES =>
                {
                    transient_attempts += 1;
                    let reason = format!("{:#}", e);

                    // Account-level limit: activate global circuit breaker (60 min pause)
                    // so all other tasks stop burning turns.
                    let backoff_secs = if reason.to_lowercase().contains(ACCOUNT_LIMIT_PATTERN) {
                        let pause = std::time::Duration::from_secs(3600);
                        store.set_rate_limit(pause).await;
                        3600u64
                    } else {
                        30u64 * (1u64 << (transient_attempts - 1).min(4))
                    };

                    tracing::warn!(
                        task_id = %id.0,
                        attempt = transient_attempts,
                        max = MAX_TRANSIENT_RETRIES,
                        backoff_secs,
                        "transient failure detected; retrying after backoff: {reason}"
                    );
                    log_task_failure_event(
                        &events,
                        &id,
                        &format!("transient (attempt {transient_attempts}): {reason}"),
                    )
                    .await;
                    if let Err(pe) = mutate_and_persist(&store, &id, |s| {
                        s.rounds.push(RoundResult {
                            turn: s.turn,
                            action: "transient_retry".into(),
                            result: format!(
                                "attempt {transient_attempts}/{MAX_TRANSIENT_RETRIES}"
                            ),
                            detail: Some(reason.clone()),
                            first_token_latency_ms: None,
                        });
                        s.status = TaskStatus::Pending;
                        s.failure_kind = None;
                        s.scheduler.mark_retry_backoff();
                        s.error = Some(format!(
                            "retrying after transient failure (attempt {transient_attempts}): {reason}"
                        ));
                    })
                    .await
                    {
                        tracing::error!("failed to record retry for task {id:?}: {pe}");
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                }
                err @ Err(_) => break err,
            }
        };

        // Cleanup workspace when task ends.
        // On failure: always remove to prevent stale worktrees from polluting subsequent
        // tasks (the root cause of cross-task PR pollution, issue #799).
        // On success: respect auto_cleanup so users can inspect the worktree post-run.
        if let Some(wmgr) = workspace_mgr {
            // Also force cleanup when the task ended with Failed status even though the
            // executor returned Ok(()) — the worktree-collision path sets TaskStatus::Failed
            // then returns Ok(ImplementOutcome::Done) so task_result.is_err() is false.
            let task_is_failed = store
                .get(&id)
                .is_some_and(|s| s.status == TaskStatus::Failed);
            if task_result.is_err() || task_is_failed || wmgr.config.auto_cleanup {
                if let Err(e) = wmgr.remove_workspace(&id).await {
                    tracing::warn!("workspace cleanup failed for {id:?}: {e}");
                }
            }
        }

        task_result
    });

    // Store abort handle before the watcher consumes the JoinHandle, so the
    // cancel endpoint can abort the task's Tokio future (which also kills the
    // child process via kill_on_drop(true)).
    store_for_abort.store_abort_handle(&id_for_abort, handle.abort_handle());

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, e.to_string())
                    .await;
            }
            Err(join_err) if join_err.is_cancelled() => {
                // abort() was called by the cancel endpoint; status already set to
                // Cancelled before abort() was called — do not overwrite with Failed.
                tracing::info!("task {id_watcher:?} cancelled via abort");
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked: {join_err}");
                let reason = format!("task failed unexpectedly: {join_err}");
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, reason).await;
            }
        }
        store_watcher.remove_abort_handle(&id_watcher);
        // Close the stream channel so SSE clients receive EOF.
        store_watcher.close_task_stream(&id_watcher);
        if let Some(cb) = completion_callback {
            match store_watcher.get(&id_watcher) {
                Some(final_state) => cb(final_state).await,
                None => tracing::warn!(
                    "completion_callback: task {:?} not found in store after completion",
                    id_watcher
                ),
            }
        }
    });

    task_id
}

/// Create a task that waits for its dependencies before starting.
///
/// If all deps are already Done, registers as Pending immediately.
/// If deps are unresolved, registers as AwaitingDeps.
/// Returns Err if a circular dependency is detected.
pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    let depends_on = req.depends_on.clone();
    let task_id = TaskId::new();

    if detect_cycle(&store, &task_id, &depends_on) {
        anyhow::bail!("circular dependency detected for task {}", task_id.0);
    }

    let mut all_done = true;
    for dep_id in &depends_on {
        if !matches!(store.dep_status(dep_id).await, Some(TaskStatus::Done)) {
            all_done = false;
            break;
        }
    }

    let mut state = TaskState::new(task_id.clone());
    state.task_kind = classify_task_kind(&req);
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.phase = state.task_kind.default_phase();
    state.description = summarize_request_description(&req);
    state.request_settings = Some(PersistedRequestSettings::from_req(&req));
    // Persist the caller's resolved project root so that duplicate detection
    // (which keys on project_root + external_id) and the dep-watcher's project
    // path resolution both work correctly for waiting tasks.
    state.project_root = req.project.clone();
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
        state.scheduler = crate::task_runner::TaskSchedulerState::awaiting_dependencies();
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
/// Returns the IDs of tasks that were transitioned to Pending.
///
/// Async because dependency status checks fall back to the database for
/// terminal tasks (Done/Failed) that were evicted from the startup cache.
///
/// Returns `(ready_ids, newly_failed_ids)`.  Both sets must be persisted by
/// the caller so that status transitions survive a process restart.
pub async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
    let mut ready = Vec::new();
    // (task_id, dep_id, dep_terminal_status_label) — label is "failed" or "cancelled".
    let mut failed_deps: Vec<(TaskId, TaskId, &'static str)> = Vec::new();

    // Snapshot AwaitingDeps tasks from cache before async work.
    let awaiting: Vec<(TaskId, Vec<TaskId>)> = store
        .cache
        .iter()
        .filter_map(|e| {
            let task = e.value();
            if matches!(task.status, TaskStatus::AwaitingDeps) {
                Some((task.id.clone(), task.depends_on.clone()))
            } else {
                None
            }
        })
        .collect();

    for (task_id, depends_on) in awaiting {
        // Single pass: one DB lookup per dep (cache-first, then lightweight
        // `SELECT status` — no `rounds` JSON decode).
        let mut failed_dep_id: Option<(TaskId, &'static str)> = None;
        let mut all_done = true;
        for dep_id in &depends_on {
            match store.dep_status(dep_id).await {
                Some(TaskStatus::Failed) => {
                    failed_dep_id = Some((dep_id.clone(), "failed"));
                    break; // fail-fast — no need to inspect remaining deps
                }
                Some(TaskStatus::Cancelled) => {
                    // A cancelled dependency hard-fails its dependents. When a
                    // task is re-queued it always receives a new TaskId, so the
                    // original (now-cancelled) TaskId in `depends_on` can never
                    // transition to Done. Leaving the dependent in AwaitingDeps
                    // would block it indefinitely; failing it immediately lets
                    // the caller re-submit with correct dependency IDs.
                    failed_dep_id = Some((dep_id.clone(), "cancelled"));
                    break; // fail-fast — no need to inspect remaining deps
                }
                Some(TaskStatus::Done) => {} // this dep is satisfied
                _ => {
                    // Pending, Implementing, or unknown — not ready yet.
                    // Keep scanning in case a later dep is Failed.
                    all_done = false;
                }
            }
        }
        if let Some((fd, label)) = failed_dep_id {
            failed_deps.push((task_id, fd, label));
            continue;
        }
        if all_done {
            ready.push(task_id);
        }
    }

    let mut newly_failed: Vec<TaskId> = Vec::new();
    for (task_id, failed_dep_id, label) in &failed_deps {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            // Only overwrite if the task is still AwaitingDeps — a concurrent
            // cancel or status transition must not be clobbered.
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Failed;
                entry.scheduler.mark_terminal(&TaskStatus::Failed);
                entry.error = Some(format!("dependency {} {}", failed_dep_id.0, label));
                newly_failed.push(task_id.clone());
            }
        }
    }
    // Only return IDs where the transition actually happened.  If a task was
    // cancelled or otherwise moved out of AwaitingDeps between the snapshot
    // and this guard, it must NOT be included — the dep-watcher would otherwise
    // launch an already-terminal or concurrently-cancelled task.
    let mut actually_ready: Vec<TaskId> = Vec::with_capacity(ready.len());
    for task_id in &ready {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Pending;
                entry.scheduler.clear_to_queued();
                actually_ready.push(task_id.clone());
            }
        }
    }

    (actually_ready, newly_failed)
}

#[cfg(test)]
mod tests {
    use super::super::request::SystemTaskInput;
    use super::*;
    use async_trait::async_trait;
    use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
    use harness_core::types::{Capability, ContextItem, EventFilters, ExecutionPhase, TokenUsage};
    use tokio::time::{sleep, Duration, Instant};

    fn tid(s: &str) -> harness_core::types::TaskId {
        harness_core::types::TaskId(s.to_string())
    }

    struct CapturingAgent {
        captured: tokio::sync::Mutex<Vec<ContextItem>>,
    }

    impl CapturingAgent {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                captured: tokio::sync::Mutex::new(Vec::new()),
            })
        }
    }

    async fn wait_until(
        timeout: Duration,
        mut predicate: impl FnMut() -> bool,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("condition not met within {:?}", timeout);
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    #[async_trait]
    impl harness_core::agent::CodeAgent for CapturingAgent {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
            Ok(AgentResponse {
                output: String::new(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: 0.0,
                },
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            // Mirror execute(): capture context on first call so tests that
            // verify skill injection work whether execute or execute_stream is called.
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut skill_store = harness_skills::store::SkillStore::new();
        skill_store.create(
            "test-skill".to_string(),
            "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
        );
        let skills = Arc::new(RwLock::new(skill_store));

        let agent = CapturingAgent::new();
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("test task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
            None,
        )
        .await;

        wait_until(Duration::from_secs(3), || {
            agent
                .captured
                .try_lock()
                .map(|captured| !captured.is_empty())
                .unwrap_or(false)
        })
        .await?;

        let captured = agent.captured.lock().await;
        assert!(
            !captured.is_empty(),
            "expected skills to be injected into AgentRequest.context"
        );
        assert!(
            captured
                .iter()
                .any(|item| matches!(item, ContextItem::Skill { .. })),
            "expected at least one ContextItem::Skill"
        );
        Ok(())
    }

    struct BlockingInterceptor;

    #[async_trait]
    impl harness_core::interceptor::TurnInterceptor for BlockingInterceptor {
        fn name(&self) -> &str {
            "blocking-test"
        }

        async fn pre_execute(
            &self,
            _req: &AgentRequest,
        ) -> harness_core::interceptor::InterceptResult {
            harness_core::interceptor::InterceptResult::block("test block")
        }
    }

    #[tokio::test]
    async fn blocking_interceptor_fails_task() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
            vec![Arc::new(BlockingInterceptor)];

        let req = CreateTaskRequest {
            prompt: Some("blocked task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        let task_id = spawn_task(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events,
            interceptors,
            req,
            None,
            permit,
            None,
            None,
        )
        .await;

        wait_until(Duration::from_secs(3), || {
            store
                .get(&task_id)
                .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
        })
        .await?;

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Blocked by interceptor"),
            "error message should mention blocked: {:?}",
            state.error
        );
        Ok(())
    }

    /// Verify that a local u32 counter correctly tracks waiting rounds without any store query.
    /// Task execution is sequential within a single tokio task, so a plain local counter suffices.
    #[test]
    fn local_waiting_counter_increments_on_each_waiting_response() {
        let max_rounds = 5u32;
        let mut waiting_count: u32 = 0;
        let mut observed: Vec<u32> = Vec::new();

        // Simulate the initial wait before the review loop.
        waiting_count += 1;
        observed.push(waiting_count);

        // Simulate inter-round waits (max_rounds - 1 additional waits).
        for _ in 1..max_rounds {
            waiting_count += 1;
            observed.push(waiting_count);
        }

        let expected: Vec<u32> = (1..=max_rounds).collect();
        assert_eq!(
            observed, expected,
            "waiting_count must increment monotonically on each waiting response"
        );
    }

    #[tokio::test]
    async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let req = CreateTaskRequest {
            prompt: Some("panic path".into()),
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        let task_id = spawn_task_with_worktree_detector(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events.clone(),
            vec![],
            req,
            || -> PathBuf {
                panic!("forced detect_main_worktree panic");
            },
            None,
            permit,
            None,
            None,
            None,
            None,
        )
        .await;

        wait_until(Duration::from_secs(3), || {
            store
                .get(&task_id)
                .is_some_and(|state| matches!(state.status, TaskStatus::Failed))
        })
        .await?;

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("detect_main_worktree panicked"),
            "expected panic reason in task error, got: {error}"
        );
        assert!(
            error.contains("forced detect_main_worktree panic"),
            "expected panic payload in task error, got: {error}"
        );

        let expected_detail = format!("task_id={}", task_id.0);
        let failure_events = events
            .query(&EventFilters {
                hook: Some("task_failure".to_string()),
                ..Default::default()
            })
            .await?;
        assert!(
            failure_events.iter().any(|event| {
                event.detail.as_deref() == Some(expected_detail.as_str())
                    && event
                        .reason
                        .as_deref()
                        .unwrap_or_default()
                        .contains("forced detect_main_worktree panic")
            }),
            "expected task_failure event containing panic payload, got: {:?}",
            failure_events
        );
        Ok(())
    }

    #[tokio::test]
    async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let req = CreateTaskRequest {
            issue: Some(20),
            source: Some("github".to_string()),
            external_id: Some("20".to_string()),
            repo: Some("acme/harness".to_string()),
            ..Default::default()
        };

        let task_id = register_pending_task(store.clone(), &req).await;
        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;

        assert_eq!(state.source.as_deref(), Some("github"));
        assert_eq!(state.external_id.as_deref(), Some("20"));
        assert_eq!(state.repo.as_deref(), Some("acme/harness"));
        assert_eq!(state.description.as_deref(), Some("issue #20"));
        Ok(())
    }

    /// Mock agent that records the `execution_phase` from every call and
    /// returns pre-configured responses in order.
    struct PhaseCapturingAgent {
        phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
        responses: tokio::sync::Mutex<Vec<String>>,
    }

    impl PhaseCapturingAgent {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                phases: tokio::sync::Mutex::new(Vec::new()),
                responses: tokio::sync::Mutex::new(responses),
            })
        }

        async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
            self.phases.lock().await.clone()
        }

        async fn next_response(&self) -> String {
            let mut guard = self.responses.lock().await;
            if guard.is_empty() {
                String::new()
            } else {
                guard.remove(0)
            }
        }
    }

    #[async_trait]
    impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
        fn name(&self) -> &str {
            "phase-capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            Ok(AgentResponse {
                output,
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            if !output.is_empty() {
                if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                    tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
                }
            }
            if let Err(e) = tx.send(StreamItem::Done).await {
                tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn planning_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Agent returns empty output (no PR URL) → task completes after implementation.
        let agent = PhaseCapturingAgent::new(vec![String::new()]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            !phases.is_empty(),
            "expected at least one agent call, got none"
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "initial implementation turn must use Planning phase"
        );
        Ok(())
    }

    #[tokio::test]
    async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Call 1 (execute_stream): return a PR URL to trigger the review loop.
        // Call 2 (execute): return LGTM to complete the review loop.
        let agent = PhaseCapturingAgent::new(vec![
            "PR_URL=https://github.com/owner/repo/pull/1".into(),
            "LGTM".into(),
        ]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(1),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            phases.len() >= 2,
            "expected at least 2 agent calls (implementation + review check), got {}",
            phases.len()
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "implementation turn must use Planning phase"
        );
        assert_eq!(
            phases[1],
            Some(ExecutionPhase::Execution),
            "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
        );
        Ok(())
    }

    #[test]
    fn preregistered_metadata_refresh_preserves_persisted_phase() {
        let mut state = TaskState::new(TaskId::new());
        state.phase = TaskPhase::Plan;

        let req = CreateTaskRequest {
            prompt: Some("small prompt".into()),
            project: Some(PathBuf::from("/tmp/recovered-project")),
            repo: Some("owner/repo".into()),
            ..Default::default()
        };

        refresh_preregistered_task_metadata(
            &mut state,
            &req,
            PathBuf::from("/tmp/recovered-project"),
            Some("small prompt".into()),
        );

        assert_eq!(state.phase, TaskPhase::Plan);
        assert_eq!(state.repo.as_deref(), Some("owner/repo"));
        assert_eq!(state.description.as_deref(), Some("small prompt"));
    }

    #[test]
    fn transient_error_detection() {
        // Positive cases — should match transient patterns.
        assert!(is_transient_error(
            "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
        ));
        assert!(is_transient_error("rate limit exceeded, retry after 30s"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("502 Bad Gateway"));
        assert!(is_transient_error(
            "Agent stream stalled: no output for 300s"
        ));
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error(
            "stream idle timeout after 300s: zombie connection terminated"
        ));
        assert!(is_transient_error(
            "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
        ));

        // Negative cases — permanent errors should not match.
        assert!(!is_transient_error(
            "Task did not receive LGTM after 5 review rounds."
        ));
        assert!(!is_transient_error(
            "triage output unparseable — agent did not produce TRIAGE=<decision>"
        ));
        assert!(!is_transient_error("all parallel subtasks failed"));
        assert!(!is_transient_error(
            "task failed unexpectedly: task 102 panicked"
        ));
        assert!(!is_transient_error(
            "budget exceeded: spent $5.00, limit $3.00"
        ));
    }

    #[test]
    fn parse_pr_url_standard() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/42")
        else {
            panic!("expected Some for standard GitHub PR URL");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 42);
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let Some((owner, repo, number)) =
            parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
        else {
            panic!("expected Some for PR URL with fragment");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/7/")
        else {
            panic!("expected Some for PR URL with trailing slash");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_invalid_returns_none() {
        assert!(parse_pr_url("https://github.com/acme/myrepo").is_none());
        assert!(parse_pr_url("not-a-url").is_none());
        assert!(parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
    }

    // --- planning gate ---

    #[test]
    fn short_prompt_does_not_require_plan() {
        let prompt = "Fix the typo in README.md";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn long_prompt_over_200_words_requires_plan() {
        let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
        let prompt = words.join(" ");
        assert!(prompt_requires_plan(&prompt));
    }

    #[test]
    fn prompt_with_three_file_paths_requires_plan() {
        let prompt =
            "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
        assert!(prompt_requires_plan(prompt));
    }

    #[test]
    fn prompt_with_two_file_paths_does_not_require_plan() {
        let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn xml_closing_tags_are_not_counted_as_file_paths() {
        // `wrap_external_data` wraps content in <external_data>...</external_data>.
        // Three `</external_data>` closing tags contain '/' but must NOT trigger
        // the planning gate — they are markup, not file paths.
        let prompt = "GC applied files:\n\
                      <external_data>test-guard.sh</external_data>\n\
                      Rationale:\n<external_data>test</external_data>\n\
                      Validation:\n<external_data>test</external_data>";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn non_decomposable_source_list_includes_periodic_and_sprint() {
        assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
        assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
        assert!(!is_non_decomposable_prompt_source(Some("github")));
        assert!(!is_non_decomposable_prompt_source(None));
    }

    #[test]
    fn request_task_kind_trusts_only_internal_system_input() {
        let spoofed = CreateTaskRequest {
            prompt: Some("review this".to_string()),
            source: Some("periodic_review".to_string()),
            ..Default::default()
        };
        assert_eq!(spoofed.task_kind(), TaskKind::Prompt);

        let trusted = CreateTaskRequest {
            prompt: Some("review this".to_string()),
            source: Some("periodic_review".to_string()),
            system_input: Some(SystemTaskInput::PeriodicReview {
                prompt: "review this".to_string(),
            }),
            ..Default::default()
        };
        assert_eq!(trusted.task_kind(), TaskKind::Review);
    }

    #[test]
    fn system_input_for_request_clones_only_explicit_internal_metadata() {
        let spoofed = CreateTaskRequest {
            prompt: Some("review this".to_string()),
            source: Some("periodic_review".to_string()),
            ..Default::default()
        };
        assert_eq!(spoofed.system_input.clone(), None);

        let trusted = CreateTaskRequest {
            prompt: Some("review this".to_string()),
            source: Some("periodic_review".to_string()),
            system_input: Some(SystemTaskInput::PeriodicReview {
                prompt: "review this".to_string(),
            }),
            ..Default::default()
        };
        assert_eq!(
            trusted.system_input.clone(),
            Some(SystemTaskInput::PeriodicReview {
                prompt: "review this".to_string(),
            })
        );
    }

    // --- dependency scheduling tests ---

    #[tokio::test]
    async fn spawn_awaiting_all_deps_done_creates_pending() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let req = CreateTaskRequest {
            prompt: Some("task with done dep".into()),
            depends_on: vec![dep_id],
            ..Default::default()
        };

        let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
        let state = store.get(&task_id).expect("task should be in store");
        assert!(
            matches!(state.status, TaskStatus::Pending),
            "expected Pending when all deps Done, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_unresolved_dep_creates_awaiting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-pending");
        let dep = TaskState::new(dep_id.clone()); // Pending by default
        store.insert(&dep).await;

        let req = CreateTaskRequest {
            prompt: Some("task with pending dep".into()),
            depends_on: vec![dep_id],
            ..Default::default()
        };

        let task_id = spawn_task_awaiting_deps(store.clone(), req).await?;
        let state = store.get(&task_id).expect("task should be in store");
        assert!(
            matches!(state.status, TaskStatus::AwaitingDeps),
            "expected AwaitingDeps when dep not Done, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_detects_direct_cycle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        // Chain: new → A → new  (A's depends_on contains new_id)
        let new_id = tid("new-task");
        let a_id = tid("task-a");

        let mut task_a = TaskState::new(a_id.clone());
        task_a.depends_on = vec![new_id.clone()];
        store.insert(&task_a).await;

        assert!(
            detect_cycle(&store, &new_id, &[a_id]),
            "expected direct cycle to be detected"
        );
        Ok(())
    }

    #[tokio::test]
    async fn spawn_awaiting_detects_transitive_cycle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        // Chain: new → A → B → new
        let new_id = tid("new-task");
        let a_id = tid("task-a");
        let b_id = tid("task-b");

        let mut task_b = TaskState::new(b_id.clone());
        task_b.depends_on = vec![new_id.clone()];
        store.insert(&task_b).await;

        let mut task_a = TaskState::new(a_id.clone());
        task_a.depends_on = vec![b_id];
        store.insert(&task_a).await;

        assert!(
            detect_cycle(&store, &new_id, &[a_id]),
            "expected transitive cycle to be detected"
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_no_tasks_returns_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "expected no ready tasks");
        assert!(failed.is_empty(), "expected no failed tasks");
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_ready_transitions_to_pending() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.contains(&task_id), "expected task in ready_ids");
        assert!(failed.is_empty(), "expected no failed tasks");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Pending),
            "expected Pending after transition, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_failed_dep_transitions_to_failed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-failed");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Failed;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "expected no ready tasks");
        assert!(failed.contains(&task_id), "expected task in failed_ids");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed after dep failure, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_partial_deps_stays_awaiting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_done_id = tid("dep-done");
        let mut dep_done = TaskState::new(dep_done_id.clone());
        dep_done.status = TaskStatus::Done;
        store.insert(&dep_done).await;

        let dep_pending_id = tid("dep-pending");
        let dep_pending = TaskState::new(dep_pending_id.clone()); // Pending by default
        store.insert(&dep_pending).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_done_id, dep_pending_id];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(!ready.contains(&task_id), "task must not be in ready_ids");
        assert!(!failed.contains(&task_id), "task must not be in failed_ids");

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::AwaitingDeps),
            "expected AwaitingDeps with partial deps, got {:?}",
            state.status
        );
        Ok(())
    }

    /// A cancelled dependency hard-fails its dependents. Re-queued tasks always
    /// get new TaskIds, so the old cancelled ID would never resolve — leaving the
    /// dependent in AwaitingDeps would block it indefinitely.
    #[tokio::test]
    async fn check_awaiting_cancelled_dep_hard_fails_dependent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-cancelled");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Cancelled;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id.clone()];
        store.insert(&task).await;

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(ready.is_empty(), "cancelled dep must not unblock dependent");
        assert!(
            failed.contains(&task_id),
            "cancelled dep must hard-fail its dependent"
        );

        let state = store.get(&task_id).expect("task should still be in store");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "dependent must be Failed when dep is Cancelled, got {:?}",
            state.status
        );
        Ok(())
    }

    #[tokio::test]
    async fn check_awaiting_concurrent_cancel_excluded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let dep_id = tid("dep-done");
        let mut dep = TaskState::new(dep_id.clone());
        dep.status = TaskStatus::Done;
        store.insert(&dep).await;

        let task_id = tid("awaiting-task");
        let mut task = TaskState::new(task_id.clone());
        task.status = TaskStatus::AwaitingDeps;
        task.depends_on = vec![dep_id];
        store.insert(&task).await;

        // Simulate concurrent cancel: flip status before check_awaiting_deps runs.
        if let Some(mut entry) = store.cache.get_mut(&task_id) {
            entry.status = TaskStatus::Cancelled;
        }

        let (ready, failed) = check_awaiting_deps(&store).await;
        assert!(
            !ready.contains(&task_id),
            "cancelled task must not be in ready_ids"
        );
        assert!(
            !failed.contains(&task_id),
            "cancelled task must not be in failed_ids"
        );
        Ok(())
    }
}
