use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::http::AppState;
use crate::task_runner::{TaskFailureKind, TaskId, TaskStatus};

pub mod feishu;
pub mod github_issues;

/// Normalized issue from any intake source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingIssue {
    /// Source identifier: "github", "feishu", "dashboard".
    pub source: String,
    /// Source-specific unique ID (e.g. GitHub issue number, feishu message_id).
    pub external_id: String,
    /// Human-readable identifier (e.g. "#42", "feishu-msg-xxx").
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    /// Repository slug "owner/repo" for GitHub sources.
    pub repo: Option<String>,
    pub url: Option<String>,
    pub priority: Option<i32>,
    pub labels: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    /// Project root override for this issue's repo.
    #[serde(default)]
    pub project_root: Option<std::path::PathBuf>,
}

/// Result passed to `IntakeSource::on_task_complete`.
pub struct TaskCompletionResult {
    pub status: TaskStatus,
    pub failure_kind: Option<TaskFailureKind>,
    pub pr_url: Option<String>,
    pub error: Option<String>,
    pub summary: String,
}

/// Trait for intake channels. Each channel polls or listens and produces `IncomingIssue`s.
#[async_trait]
pub trait IntakeSource: Send + Sync {
    fn name(&self) -> &str;

    /// Fetch new issues that haven't been dispatched yet.
    async fn poll(&self) -> anyhow::Result<Vec<IncomingIssue>>;

    /// Mark an issue as dispatched so it won't be returned by future polls.
    async fn mark_dispatched(&self, external_id: &str, task_id: &TaskId) -> anyhow::Result<()>;

    /// Remove an issue from the dispatched set (e.g. on enqueue failure).
    async fn unmark_dispatched(&self, external_id: &str);

    /// Called when a task spawned from this source reaches a terminal state.
    async fn on_task_complete(
        &self,
        external_id: &str,
        result: &TaskCompletionResult,
    ) -> anyhow::Result<()>;
}

/// Orchestrates all registered intake sources with sprint-planned dispatch.
///
/// Instead of dispatching issues immediately, collects all new issues, runs a
/// sprint planner agent to group them into rounds, then executes rounds sequentially
/// with parallel dispatch within each round.
pub struct IntakeOrchestrator {
    sources: Vec<Arc<dyn IntakeSource>>,
    poll_interval: Duration,
    planner_agent: Option<String>,
    sprint_timeout: Duration,
    retry_backoff_base: Duration,
    retry_backoff_max: Duration,
}

/// Polling interval when waiting for task completion.
const TASK_POLL_INTERVAL: Duration = Duration::from_secs(15);

impl IntakeOrchestrator {
    pub fn new(
        sources: Vec<Arc<dyn IntakeSource>>,
        poll_interval: Duration,
        planner_agent: Option<String>,
        sprint_timeout: Duration,
        retry_backoff_base: Duration,
        retry_backoff_max: Duration,
    ) -> Self {
        Self {
            sources,
            poll_interval,
            planner_agent,
            sprint_timeout,
            retry_backoff_base,
            retry_backoff_max,
        }
    }

    /// Spawn the poll loop as a background task. No-op if there are no sources.
    pub fn start(self, state: Arc<AppState>) {
        if self.sources.is_empty() {
            tracing::debug!("intake: no sources configured, poller not started");
            return;
        }
        tracing::info!(
            source_count = self.sources.len(),
            poll_interval_secs = self.poll_interval.as_secs(),
            "intake orchestrator started"
        );
        tokio::spawn(async move {
            let mw = &state.core.server.config.maintenance_window;
            let mut was_in_window = state
                .core
                .maintenance_active
                .load(std::sync::atomic::Ordering::Relaxed);
            loop {
                tokio::time::sleep(self.poll_interval).await;
                let in_window = mw.in_quiet_window(chrono::Utc::now());
                if in_window != was_in_window {
                    state
                        .core
                        .maintenance_active
                        .store(in_window, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(in_window, "maintenance window state changed");
                    was_in_window = in_window;
                }
                if in_window {
                    tracing::debug!("intake: quiet window active, skipping tick");
                    continue;
                }
                self.poll_tick(&state).await;
            }
        });
    }

    async fn poll_tick(&self, state: &Arc<AppState>) {
        // 1. Collect all new issues from all sources, grouped by repo.
        type RepoIssues = Vec<(Arc<dyn IntakeSource>, IncomingIssue)>;
        let mut by_repo: std::collections::HashMap<String, RepoIssues> =
            std::collections::HashMap::new();

        for source in &self.sources {
            let issues = match source.poll().await {
                Ok(issues) => issues,
                Err(e) => {
                    tracing::error!(source = source.name(), "intake poll failed: {e}");
                    continue;
                }
            };
            for issue in issues {
                let repo_key = issue.repo.clone().unwrap_or_else(|| "default".to_string());
                by_repo
                    .entry(repo_key)
                    .or_default()
                    .push((Arc::clone(source), issue));
            }
        }

        if by_repo.is_empty() {
            return;
        }

        // 2. Run a separate sprint planner per repo, all in parallel.
        let mut handles = Vec::new();
        for (repo, issues) in by_repo {
            let project_root = issues
                .first()
                .and_then(|(_, i)| i.project_root.clone())
                .unwrap_or_else(|| state.core.project_root.clone());
            let project_id =
                match crate::task_runner::resolve_canonical_project(Some(project_root)).await {
                    Ok(path) => path.to_string_lossy().into_owned(),
                    Err(_) => state.core.project_root.to_string_lossy().into_owned(),
                };
            if let Some(workflows) = state.core.project_workflow_store.as_ref() {
                if let Err(e) = workflows
                    .record_poll_started(&project_id, Some(&repo))
                    .await
                {
                    tracing::warn!(
                        repo,
                        "intake: failed to mark project workflow poll state: {e}"
                    );
                }
            }
            let state = Arc::clone(state);
            let planner_agent = self.planner_agent.clone();
            let sprint_timeout = self.sprint_timeout;
            let retry_backoff_base = self.retry_backoff_base;
            let retry_backoff_max = self.retry_backoff_max;
            handles.push(tokio::spawn(async move {
                run_repo_sprint(
                    &state,
                    &repo,
                    issues,
                    planner_agent.as_deref(),
                    sprint_timeout,
                    retry_backoff_base,
                    retry_backoff_max,
                )
                .await;
            }));
        }

        // Wait for all repo sprints to finish.
        for handle in handles {
            if let Err(e) = handle.await {
                tracing::error!("intake: repo sprint task panicked: {e}");
            }
        }
    }
}

/// Validate the sprint DAG for missing upstream dependencies and cycles.
///
/// `all_task_issues` — the full set of issue numbers the sprint will execute.
/// `deps` — mapping from issue number to its (already-filtered) dependency list.
///
/// Skipped issues must be removed from dep lists before calling this function,
/// so that a task depending on a skipped issue is not incorrectly flagged.
fn validate_dag(
    all_task_issues: &std::collections::HashSet<u64>,
    deps: &std::collections::HashMap<u64, Vec<u64>>,
) -> Result<(), String> {
    let mut errors: Vec<String> = Vec::new();

    // Check for deps that reference issues not in the sprint plan.
    for (&issue, dep_list) in deps {
        for &dep in dep_list {
            if !all_task_issues.contains(&dep) {
                errors.push(format!(
                    "issue {issue} depends on {dep} which is not in the sprint plan"
                ));
            }
        }
    }

    // Kahn's algorithm: detect cycles among the plan issues.
    // Build in-degree map and downstream adjacency list.
    let mut in_degree: std::collections::HashMap<u64, usize> =
        all_task_issues.iter().map(|&id| (id, 0usize)).collect();
    let mut dependents: std::collections::HashMap<u64, Vec<u64>> =
        all_task_issues.iter().map(|&id| (id, Vec::new())).collect();

    for (&issue, dep_list) in deps {
        for &dep in dep_list {
            if all_task_issues.contains(&dep) {
                // Edge direction: dep → issue (dep must complete first)
                *in_degree.entry(issue).or_insert(0) += 1;
                dependents.entry(dep).or_default().push(issue);
            }
        }
    }

    // BFS starting from zero-in-degree nodes.
    let mut queue: std::collections::VecDeque<u64> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut processed = 0usize;
    while let Some(node) = queue.pop_front() {
        processed += 1;
        for &dependent in dependents.get(&node).into_iter().flatten() {
            let deg = in_degree.entry(dependent).or_insert(0);
            *deg = deg.saturating_sub(1);
            if *deg == 0 {
                queue.push_back(dependent);
            }
        }
    }

    if processed < all_task_issues.len() {
        // Nodes remaining with in-degree > 0 are participants in a cycle.
        let mut cycle_nodes: Vec<u64> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&id, _)| id)
            .collect();
        cycle_nodes.sort_unstable();
        errors.push(format!("cycle detected among issues: {cycle_nodes:?}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedTaskSkip {
    issue: u64,
    upstream: u64,
    reason: String,
}

#[derive(Debug, Clone, PartialEq)]
struct SprintPlanNormalization {
    tasks: Vec<harness_core::prompts::SprintTask>,
    dropped_completed_edges: Vec<(u64, u64)>,
    skipped_tasks: Vec<NormalizedTaskSkip>,
}

fn normalize_sprint_tasks(
    tasks: Vec<harness_core::prompts::SprintTask>,
    planner_skips: &std::collections::HashSet<u64>,
    completed_issues: &std::collections::HashSet<u64>,
) -> SprintPlanNormalization {
    let mut tasks = tasks;
    let mut removed_tasks: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut dropped_completed_edges = Vec::new();
    let mut skipped_tasks = Vec::new();

    loop {
        let current_issues: std::collections::HashSet<u64> =
            tasks.iter().map(|t| t.issue).collect();
        let mut changed = false;
        let mut next_tasks = Vec::with_capacity(tasks.len());

        for mut task in tasks {
            let mut filtered_deps = Vec::with_capacity(task.depends_on.len());
            let mut skip_detail: Option<NormalizedTaskSkip> = None;

            for dep in task.depends_on {
                if planner_skips.contains(&dep) {
                    continue;
                }
                if removed_tasks.contains(&dep) {
                    skip_detail = Some(NormalizedTaskSkip {
                        issue: task.issue,
                        upstream: dep,
                        reason: format!(
                            "upstream issue {dep} was removed from this sprint during intake normalization"
                        ),
                    });
                    break;
                }
                if current_issues.contains(&dep) {
                    filtered_deps.push(dep);
                    continue;
                }
                if completed_issues.contains(&dep) {
                    dropped_completed_edges.push((task.issue, dep));
                    changed = true;
                    continue;
                }
                skip_detail = Some(NormalizedTaskSkip {
                    issue: task.issue,
                    upstream: dep,
                    reason: format!(
                        "upstream issue {dep} is outside the sprint plan and not completed"
                    ),
                });
                break;
            }

            if let Some(skip) = skip_detail {
                removed_tasks.insert(task.issue);
                skipped_tasks.push(skip);
                changed = true;
                continue;
            }

            task.depends_on = filtered_deps;
            next_tasks.push(task);
        }

        if !changed {
            return SprintPlanNormalization {
                tasks: next_tasks,
                dropped_completed_edges,
                skipped_tasks,
            };
        }

        tasks = next_tasks;
    }
}

fn parse_issue_external_id(external_id: &str) -> Option<u64> {
    external_id
        .strip_prefix("issue:")
        .unwrap_or(external_id)
        .parse()
        .ok()
}

fn latest_timestamp_at_least(candidate: Option<&str>, existing: Option<&str>) -> bool {
    match (candidate, existing) {
        (Some(candidate), Some(existing)) => candidate >= existing,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    }
}

fn latest_unblocking_issue_numbers(
    external_statuses: std::collections::HashMap<
        String,
        (Option<String>, crate::task_runner::TaskStatus),
    >,
) -> std::collections::HashSet<u64> {
    let mut latest_by_issue: std::collections::HashMap<
        u64,
        (Option<String>, crate::task_runner::TaskStatus),
    > = std::collections::HashMap::new();

    for (external_id, (created_at, status)) in external_statuses {
        let Some(issue) = parse_issue_external_id(&external_id) else {
            continue;
        };
        let should_replace = latest_by_issue
            .get(&issue)
            .map(|(existing_created_at, _)| {
                latest_timestamp_at_least(created_at.as_deref(), existing_created_at.as_deref())
            })
            .unwrap_or(true);
        if should_replace {
            latest_by_issue.insert(issue, (created_at, status));
        }
    }

    latest_by_issue
        .into_iter()
        .filter_map(|(issue, (_, status))| status_unblocks_dependents(&status).then_some(issue))
        .collect()
}

async fn completed_issue_numbers_for_project(
    tasks: &crate::task_runner::TaskStore,
    project_id: &str,
    repo: Option<&str>,
) -> std::collections::HashSet<u64> {
    match tasks
        .list_latest_external_statuses_by_project_and_repo(project_id, repo)
        .await
    {
        Ok(external_statuses) => latest_unblocking_issue_numbers(external_statuses),
        Err(e) => {
            tracing::warn!(
                project = project_id,
                repo,
                "intake: failed to inspect latest task statuses during sprint normalization: {e}"
            );
            std::collections::HashSet::new()
        }
    }
}

fn ready_issues(
    all_task_issues: &std::collections::HashSet<u64>,
    completed: &std::collections::HashSet<u64>,
    cancelled_sprint: &std::collections::HashSet<u64>,
    running: &std::collections::HashMap<u64, TaskId>,
    deps: &std::collections::HashMap<u64, Vec<u64>>,
) -> Vec<u64> {
    all_task_issues
        .iter()
        .filter(|&&issue| {
            !completed.contains(&issue)
                && !cancelled_sprint.contains(&issue)
                && !running.contains_key(&issue)
                && deps
                    .get(&issue)
                    .map(|d| d.iter().all(|dep| completed.contains(dep)))
                    .unwrap_or(true)
        })
        .copied()
        .collect()
}

async fn record_runtime_open_issue_if_missing(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    project_id: &str,
    repo: &str,
    issue: &IncomingIssue,
) {
    let Some(runtime_store) = state.core.workflow_runtime_store.as_deref() else {
        return;
    };
    let Some(issue_number) = issue.external_id.parse::<u64>().ok() else {
        return;
    };

    let exists = if let Some(workflows) = state.core.issue_workflow_store.as_ref() {
        match workflows
            .get_by_issue(project_id, Some(repo), issue_number)
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    repo,
                    issue = issue_number,
                    "intake: failed to check issue workflow before repo backlog runtime write: {error}"
                );
                false
            }
        }
    } else {
        false
    };
    if exists {
        return;
    }

    crate::workflow_runtime_repo_backlog::record_open_issue_without_workflow(
        Some(runtime_store),
        crate::workflow_runtime_repo_backlog::OpenIssueRuntimeContext {
            project_root,
            repo: Some(repo),
            issue_number,
            issue_url: issue.url.as_deref(),
        },
    )
    .await;
}

/// Run sprint planner + DAG-based slot-filling execution for a single repo.
async fn run_repo_sprint(
    state: &Arc<AppState>,
    repo: &str,
    issues: Vec<(Arc<dyn IntakeSource>, IncomingIssue)>,
    planner_agent: Option<&str>,
    sprint_timeout: Duration,
    retry_backoff_base: Duration,
    retry_backoff_max: Duration,
) {
    let project_root = issues
        .first()
        .and_then(|(_, i)| i.project_root.clone())
        .unwrap_or_else(|| state.core.project_root.clone());
    let project_id = match crate::task_runner::resolve_canonical_project(Some(project_root.clone()))
        .await
    {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!(
                repo,
                "intake: failed to canonicalize project root for workflow lookup, using raw path: {e}"
            );
            project_root.to_string_lossy().into_owned()
        }
    };

    if let Some(workflows) = state.core.project_workflow_store.as_ref() {
        if let Err(e) = workflows
            .record_planning_started(&project_id, Some(repo))
            .await
        {
            tracing::warn!(
                repo,
                "intake: failed to mark project workflow planning state: {e}"
            );
        }
    }

    tracing::info!(
        repo,
        count = issues.len(),
        "intake: planning sprint for repo"
    );

    // 1. Run sprint planner.
    let issue_summary = build_issue_summary(&issues);
    let planner_prompt = harness_core::prompts::sprint_plan_prompt(&issue_summary);

    let planner_req = crate::task_runner::CreateTaskRequest {
        prompt: Some(planner_prompt.clone()),
        agent: planner_agent.map(String::from),
        project: Some(project_root.clone()),
        source: Some("sprint_planner".to_string()),
        system_input: Some(crate::task_runner::SystemTaskInput::SprintPlanner {
            prompt: planner_prompt,
        }),
        ..Default::default()
    };

    let planner_task_id = match crate::http::task_routes::enqueue_task(state, planner_req).await {
        Ok(id) => {
            if let Some(workflows) = state.core.project_workflow_store.as_ref() {
                if let Err(e) = workflows
                    .record_planner_enqueued(&project_id, Some(repo), &id.0)
                    .await
                {
                    tracing::warn!(
                        repo,
                        task_id = %id,
                        "intake: failed to record planner enqueue on project workflow: {e}"
                    );
                }
            }
            tracing::info!(repo, task_id = %id, "intake: sprint planner enqueued");
            id
        }
        Err(e) => {
            if let Some(workflows) = state.core.project_workflow_store.as_ref() {
                if let Err(track_err) = workflows
                    .record_degraded(
                        &project_id,
                        Some(repo),
                        &format!("failed to enqueue sprint planner: {e:?}"),
                    )
                    .await
                {
                    tracing::warn!(
                        repo,
                        "intake: failed to record degraded project workflow state: {track_err}"
                    );
                }
            }
            tracing::error!(repo, "intake: failed to enqueue sprint planner: {e:?}");
            return;
        }
    };

    let Some(planner_output) =
        poll_task_output(&state.core.tasks, &planner_task_id, sprint_timeout).await
    else {
        if let Some(workflows) = state.core.project_workflow_store.as_ref() {
            if let Err(e) = workflows
                .record_degraded(&project_id, Some(repo), "sprint planner failed")
                .await
            {
                tracing::warn!(
                    repo,
                    "intake: failed to record degraded project workflow state: {e}"
                );
            }
        }
        tracing::error!(repo, task_id = %planner_task_id, "intake: sprint planner failed");
        return;
    };

    let Some(mut plan) = harness_core::prompts::parse_sprint_plan(&planner_output) else {
        if let Some(workflows) = state.core.project_workflow_store.as_ref() {
            if let Err(e) = workflows
                .record_degraded(&project_id, Some(repo), "failed to parse sprint plan")
                .await
            {
                tracing::warn!(
                    repo,
                    "intake: failed to record degraded project workflow state: {e}"
                );
            }
        }
        tracing::error!(repo, task_id = %planner_task_id, "intake: failed to parse sprint plan");
        return;
    };

    tracing::info!(
        repo,
        tasks = plan.tasks.len(),
        skipped = plan.skip.len(),
        "intake: sprint plan parsed"
    );

    // Build lookup.
    let issue_map: std::collections::HashMap<String, (Arc<dyn IntakeSource>, IncomingIssue)> =
        issues
            .into_iter()
            .map(|(src, issue)| (issue.external_id.clone(), (src, issue)))
            .collect();

    let project_id = match crate::task_runner::resolve_canonical_project(Some(project_root.clone()))
        .await
    {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!(
                repo,
                "intake: failed to canonicalize project root for slot limit lookup, using raw path: {e}"
            );
            project_root.to_string_lossy().into_owned()
        }
    };

    let completed_issues =
        completed_issue_numbers_for_project(state.core.tasks.as_ref(), &project_id, Some(repo))
            .await;
    let planner_skip_set: std::collections::HashSet<u64> =
        plan.skip.iter().map(|skip| skip.issue).collect();
    let normalized =
        normalize_sprint_tasks(plan.tasks.clone(), &planner_skip_set, &completed_issues);
    for (issue, upstream) in &normalized.dropped_completed_edges {
        tracing::info!(
            repo,
            issue = *issue,
            upstream = *upstream,
            normalization = "drop_completed_upstream_edge",
            "intake: repaired sprint dependency outside plan"
        );
    }
    for skip in &normalized.skipped_tasks {
        tracing::warn!(
            repo,
            issue = skip.issue,
            upstream = skip.upstream,
            normalization = "skip_task_missing_upstream",
            reason = %skip.reason,
            "intake: skipped task during sprint normalization"
        );
    }
    plan.tasks = normalized.tasks;

    // 2. Mark planner-directed skips.
    for skip in &plan.skip {
        let ext_id = skip.issue.to_string();
        if let Some((source, _)) = issue_map.get(&ext_id) {
            let skip_id = harness_core::types::TaskId(format!("skip-{ext_id}"));
            if let Err(e) = source.mark_dispatched(&ext_id, &skip_id).await {
                tracing::warn!(external_id = %ext_id, "intake: failed to mark skipped: {e}");
            }
            tracing::info!(repo, external_id = %ext_id, reason = %skip.reason, "intake: issue skipped");
        }
    }

    // 3. DAG slot-filling execution.
    // Track: which issues are completed, which are running, which are waiting.
    let mut completed: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Cancelled tasks: terminal but do not unblock dependents; excluded from re-dispatch.
    let mut cancelled_sprint: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut running: std::collections::HashMap<u64, TaskId> = std::collections::HashMap::new();
    let all_task_issues: std::collections::HashSet<u64> =
        plan.tasks.iter().map(|t| t.issue).collect();

    let deps: std::collections::HashMap<u64, Vec<u64>> = plan
        .tasks
        .iter()
        .map(|t| (t.issue, t.depends_on.clone()))
        .collect();

    // Validate DAG after normalization.
    if let Err(e) = validate_dag(&all_task_issues, &deps) {
        tracing::error!(repo, error = %e, "intake: invalid sprint DAG after normalization — aborting");
        return;
    }
    let start = tokio::time::Instant::now();
    let mut transient_retry_count = 0u32;
    let mut project_workflow_monitoring_started = false;

    'sprint: loop {
        // All done?
        if completed.len() + cancelled_sprint.len() >= all_task_issues.len() && running.is_empty() {
            break;
        }
        if completed.len() + cancelled_sprint.len() >= all_task_issues.len() {
            break;
        }
        if start.elapsed() > sprint_timeout {
            tracing::warn!(repo, "intake: DAG execution timed out");
            break;
        }

        // Find ready tasks: not started, not completed, not cancelled, all deps satisfied.
        let ready = ready_issues(
            &all_task_issues,
            &completed,
            &cancelled_sprint,
            &running,
            &deps,
        );

        // Fill available slots.
        let available =
            sprint_available_slots(&state.concurrency.task_queue.diagnostics(&project_id));
        if available > 0 {
            if let Some(workflows) = state.core.project_workflow_store.as_ref() {
                if let Err(e) = workflows
                    .record_dispatch_started(&project_id, Some(repo))
                    .await
                {
                    tracing::warn!(
                        repo,
                        "intake: failed to mark project workflow dispatching state: {e}"
                    );
                }
            }
        }
        for &issue_num in ready.iter().take(available) {
            let ext_id = issue_num.to_string();
            let Some((source, issue)) = issue_map.get(&ext_id) else {
                tracing::warn!(repo, external_id = %ext_id, "intake: issue not found");
                completed.insert(issue_num);
                continue;
            };

            record_runtime_open_issue_if_missing(state, &project_root, &project_id, repo, issue)
                .await;

            let placeholder_id = harness_core::types::TaskId(format!("pending-{ext_id}"));
            if let Err(e) = source.mark_dispatched(&ext_id, &placeholder_id).await {
                tracing::warn!(external_id = %ext_id, "intake: mark_dispatched failed: {e}");
                completed.insert(issue_num);
                continue;
            }

            let req = crate::task_runner::CreateTaskRequest {
                force_execute: {
                    let workflow_cfg =
                        harness_core::config::workflow::load_workflow_config(&project_root)
                            .unwrap_or_default();
                    issue
                        .labels
                        .iter()
                        .any(|label| label == &workflow_cfg.issue_workflow.force_execute_label)
                },
                issue: issue.external_id.parse().ok(),
                project: issue
                    .project_root
                    .clone()
                    .or_else(|| Some(project_root.clone())),
                source: Some(source.name().to_string()),
                external_id: Some(issue.external_id.clone()),
                repo: Some(repo.to_string()),
                labels: issue.labels.clone(),
                ..Default::default()
            };

            match crate::http::task_routes::enqueue_task(state, req).await {
                Ok(task_id) => {
                    tracing::info!(
                        repo,
                        external_id = %ext_id,
                        task_id = %task_id,
                        running = running.len() + 1,
                        "intake: dispatched (slot filled)"
                    );
                    if let Err(e) = source.mark_dispatched(&ext_id, &task_id).await {
                        tracing::warn!(external_id = %ext_id, "intake: mark_dispatched update failed: {e}");
                    }
                    running.insert(issue_num, task_id);
                    transient_retry_count = 0;
                }
                Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow { .. }) => {
                    tracing::info!(
                        repo,
                        external_id = %ext_id,
                        "intake: quiet window active, deferring sprint"
                    );
                    source.unmark_dispatched(&ext_id).await;
                    break 'sprint;
                }
                Err(crate::services::execution::EnqueueTaskError::BadRequest(e)) => {
                    tracing::error!(repo, external_id = %ext_id, "intake: failed to spawn permanently: {e}");
                    source.unmark_dispatched(&ext_id).await;
                    completed.insert(issue_num);
                }
                Err(crate::services::execution::EnqueueTaskError::Internal(e)) => {
                    tracing::warn!(
                        repo,
                        external_id = %ext_id,
                        "intake: failed to spawn transiently, will retry: {e}"
                    );
                    source.unmark_dispatched(&ext_id).await;
                }
            }
        }

        // Detect deadlock: no running tasks but unresolved, non-cancelled tasks still pending.
        if running.is_empty() {
            let pending: std::collections::HashSet<u64> = all_task_issues
                .difference(&completed)
                .copied()
                .filter(|i| !cancelled_sprint.contains(i))
                .collect();
            if !pending.is_empty() {
                let retryable_ready = ready_issues(
                    &all_task_issues,
                    &completed,
                    &cancelled_sprint,
                    &running,
                    &deps,
                );
                if !retryable_ready.is_empty() {
                    let backoff = transient_retry_delay(
                        retry_backoff_base,
                        retry_backoff_max,
                        transient_retry_count,
                    );
                    transient_retry_count = transient_retry_count.saturating_add(1);
                    tracing::warn!(
                        repo,
                        ready = ?retryable_ready,
                        pending = ?pending,
                        backoff_secs = backoff.as_secs(),
                        "intake: no tasks running but ready work remains; likely capacity or transient enqueue failure, will retry"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                tracing::error!(
                    repo,
                    stranded = ?pending,
                    "intake: DAG deadlock — tasks stranded with unresolvable deps"
                );
            }
            break;
        }

        // Wait for any running task to complete.
        if !running.is_empty() && !project_workflow_monitoring_started {
            if let Some(workflows) = state.core.project_workflow_store.as_ref() {
                if let Err(e) = workflows
                    .record_monitoring_started(&project_id, Some(repo))
                    .await
                {
                    tracing::warn!(
                        repo,
                        "intake: failed to mark project workflow monitoring state: {e}"
                    );
                }
            }
            project_workflow_monitoring_started = true;
        }
        tokio::time::sleep(TASK_POLL_INTERVAL).await;
        let mut newly_done = Vec::new();
        let mut cancelled = Vec::new();
        for (&issue_num, task_id) in &running {
            if let Some(outcome) = workflow_sprint_issue_outcome(
                state.core.issue_workflow_store.as_deref(),
                &project_id,
                repo,
                issue_num,
            )
            .await
            {
                tracing::info!(
                    repo,
                    external_id = issue_num,
                    task_id = %task_id,
                    workflow_outcome = outcome.as_str(),
                    "intake: workflow state resolved running issue outcome"
                );
                match outcome {
                    SprintIssueOutcome::UnblocksDependents => newly_done.push(issue_num),
                    SprintIssueOutcome::Cancelled => cancelled.push(issue_num),
                }
                continue;
            }
            if let Some(task) = state.core.tasks.get(task_id) {
                if task.status.is_terminal() {
                    tracing::info!(
                        repo,
                        external_id = issue_num,
                        task_id = %task_id,
                        status = task.status.as_ref(),
                        "intake: task finished"
                    );
                    if status_unblocks_dependents(&task.status) {
                        newly_done.push(issue_num);
                    } else if matches!(task.status, TaskStatus::Cancelled) {
                        cancelled.push(issue_num);
                    }
                }
            }
        }

        let had_completed = !newly_done.is_empty() || !cancelled.is_empty();
        for issue_num in cancelled {
            running.remove(&issue_num);
            cancelled_sprint.insert(issue_num);
        }
        for issue_num in newly_done {
            running.remove(&issue_num);
            completed.insert(issue_num);
        }
        if had_completed {
            transient_retry_count = 0;
        }
        if running.is_empty() {
            continue;
        }

        if had_completed {
            continue;
        }
    }

    let stranded: std::collections::HashSet<u64> = all_task_issues
        .difference(&completed)
        .copied()
        .filter(|i| !cancelled_sprint.contains(i))
        .collect();

    tracing::info!(
        repo,
        completed = completed.len(),
        stranded = stranded.len(),
        elapsed_secs = start.elapsed().as_secs(),
        "intake: repo sprint complete"
    );

    if let Some(workflows) = state.core.project_workflow_store.as_ref() {
        let result = if stranded.is_empty() {
            workflows.record_idle(&project_id, Some(repo)).await
        } else {
            workflows
                .record_degraded(
                    &project_id,
                    Some(repo),
                    &format!(
                        "repo sprint ended with unresolved tasks: {}",
                        stranded.len()
                    ),
                )
                .await
        };
        if let Err(e) = result {
            tracing::warn!(
                repo,
                "intake: failed to finalize project workflow state: {e}"
            );
        }
    }

    if !stranded.is_empty() {
        tracing::error!(
            repo,
            issues = ?stranded,
            "intake: sprint ended with unresolved tasks"
        );
    }
}

/// Format collected issues into a summary for the planner prompt.
fn build_issue_summary(issues: &[(Arc<dyn IntakeSource>, IncomingIssue)]) -> String {
    issues
        .iter()
        .map(|(_, issue)| {
            let labels = if issue.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", issue.labels.join(", "))
            };
            let repo = issue.repo.as_deref().unwrap_or("");
            format!(
                "- #{}: {}{} ({})",
                issue.external_id, issue.title, labels, repo
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn status_unblocks_dependents(status: &TaskStatus) -> bool {
    matches!(status, TaskStatus::Done | TaskStatus::Failed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SprintIssueOutcome {
    UnblocksDependents,
    Cancelled,
}

impl SprintIssueOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::UnblocksDependents => "unblocks_dependents",
            Self::Cancelled => "cancelled",
        }
    }
}

fn workflow_state_unblocks_dependents(
    state: harness_workflow::issue_lifecycle::IssueLifecycleState,
) -> bool {
    matches!(
        state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::ReadyToMerge
            | harness_workflow::issue_lifecycle::IssueLifecycleState::Done
            | harness_workflow::issue_lifecycle::IssueLifecycleState::Failed
    )
}

async fn workflow_sprint_issue_outcome(
    store: Option<&harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    project_id: &str,
    repo: &str,
    issue_num: u64,
) -> Option<SprintIssueOutcome> {
    let store = store?;
    let workflow = match store.get_by_issue(project_id, Some(repo), issue_num).await {
        Ok(workflow) => workflow,
        Err(e) => {
            tracing::warn!(
                repo,
                issue = issue_num,
                "intake: failed to load workflow state for running issue: {e}"
            );
            return None;
        }
    }?;

    if workflow_state_unblocks_dependents(workflow.state) {
        Some(SprintIssueOutcome::UnblocksDependents)
    } else if matches!(
        workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
    ) {
        Some(SprintIssueOutcome::Cancelled)
    } else {
        None
    }
}

/// Poll a task until terminal state, return its output.
async fn poll_task_output(
    store: &crate::task_runner::TaskStore,
    task_id: &TaskId,
    timeout: Duration,
) -> Option<String> {
    let start = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(TASK_POLL_INTERVAL).await;
        if start.elapsed() > timeout {
            tracing::warn!(task_id = %task_id, "intake: task polling timed out");
            return None;
        }
        let Some(task) = store.get(task_id) else {
            continue;
        };
        if !task.status.is_terminal() {
            continue;
        }
        if task.status.is_cancelled() {
            return None;
        }
        if task.status.is_failure() {
            tracing::error!(task_id = %task_id, error = ?task.error, status = task.status.as_ref(), "intake: task failed");
            return None;
        }
        let output: String = task
            .rounds
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        if output.is_empty() {
            tracing::warn!(task_id = %task_id, "intake: task completed but no output");
            return None;
        }
        return Some(output);
    }
}

fn sprint_available_slots(diag: &crate::task_queue::QueueDiagnostics) -> usize {
    let project_headroom = diag
        .project_limit
        .saturating_sub(diag.project_running + diag.project_awaiting_global);
    let global_headroom = diag.global_limit.saturating_sub(diag.global_running);
    project_headroom.min(global_headroom)
}

fn transient_retry_delay(base: Duration, max: Duration, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(6)).unwrap_or(64);
    let secs = base.as_secs().saturating_mul(u64::from(factor));
    Duration::from_secs(secs.min(max.as_secs()))
}

/// Build an `IntakeOrchestrator` from config, registering all enabled sources.
///
/// `github_sources` must be the same `Arc<dyn IntakeSource>` instances used by
/// the completion callback so that `on_task_complete` (e.g. removing a failed
/// issue from the `dispatched` map) operates on the live poller's in-memory
/// state rather than a detached clone.
pub fn build_orchestrator(
    config: &harness_core::config::intake::IntakeConfig,
    data_dir: Option<&std::path::Path>,
    feishu_intake: Option<Arc<feishu::FeishuIntake>>,
    github_sources: Vec<Arc<dyn IntakeSource>>,
    github_token: Option<String>,
) -> IntakeOrchestrator {
    let mut sources: Vec<Arc<dyn IntakeSource>> = Vec::new();
    let mut poll_interval = Duration::from_secs(30);
    let mut planner_agent = None;
    let mut sprint_timeout = Duration::from_secs(3 * 60 * 60);
    let mut retry_backoff_base = Duration::from_secs(15);
    let mut retry_backoff_max = Duration::from_secs(120);

    if let Some(gh_config) = &config.github {
        if gh_config.enabled {
            poll_interval = Duration::from_secs(gh_config.poll_interval_secs);
            planner_agent = gh_config.planner_agent.clone();
            sprint_timeout = Duration::from_secs(gh_config.sprint_timeout_secs);
            retry_backoff_base = Duration::from_secs(gh_config.retry_backoff_base_secs);
            retry_backoff_max = Duration::from_secs(
                gh_config
                    .retry_backoff_max_secs
                    .max(gh_config.retry_backoff_base_secs),
            );
            if github_sources.is_empty() {
                // Fallback: build fresh pollers when no shared instances are supplied
                // (e.g. during testing or when called without pre-built sources).
                for repo_cfg in gh_config.effective_repos() {
                    tracing::info!(
                        repo = %repo_cfg.repo,
                        label = %repo_cfg.label,
                        "intake: GitHub Issues poller registered (fallback)"
                    );
                    let poller = github_issues::GitHubIssuesPoller::new_with_token(
                        &repo_cfg,
                        data_dir,
                        github_token.clone(),
                    );
                    sources.push(Arc::new(poller));
                }
            } else {
                sources.extend(github_sources);
            }
        }
    }

    if let Some(feishu_config) = &config.feishu {
        if feishu_config.enabled {
            if !feishu::has_verification_token(feishu_config) {
                tracing::error!(
                    "intake: Feishu enabled but verification_token is missing; orchestrator skipped registration"
                );
            } else {
                let intake = feishu_intake
                    .unwrap_or_else(|| Arc::new(feishu::FeishuIntake::new(feishu_config.clone())));
                sources.push(intake);
                tracing::info!(
                    trigger_keyword = %feishu_config.trigger_keyword,
                    "intake: Feishu bot registered in orchestrator"
                );
            }
        }
    }

    IntakeOrchestrator::new(
        sources,
        poll_interval,
        planner_agent,
        sprint_timeout,
        retry_backoff_base,
        retry_backoff_max,
    )
}

pub(crate) fn build_prompt_from_issue(issue: &IncomingIssue) -> String {
    format!(
        "You are working on {source} issue {id}: {title}\n\n\
         URL: {url}\n\n\
         Description:\n{desc}\n\n\
         Instructions:\n\
         1. This is an unattended session. Do not ask humans for help.\n\
         2. Implement changes, run validation, create PR, push.\n\
         3. Only stop for true blockers (missing auth/permissions).",
        source = issue.source,
        id = issue.identifier,
        title = issue.title,
        url = issue.url.as_deref().unwrap_or("N/A"),
        desc = issue.description.as_deref().unwrap_or("No description."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_deps(pairs: &[(u64, &[u64])]) -> std::collections::HashMap<u64, Vec<u64>> {
        pairs
            .iter()
            .map(|&(issue, deps)| (issue, deps.to_vec()))
            .collect()
    }

    fn make_issues(ids: &[u64]) -> std::collections::HashSet<u64> {
        ids.iter().copied().collect()
    }

    // --- validate_dag tests ---

    #[test]
    fn validate_dag_empty() {
        assert!(validate_dag(&make_issues(&[]), &make_deps(&[])).is_ok());
    }

    #[test]
    fn validate_dag_valid_linear() {
        // A(1) → B(2) → C(3)
        let issues = make_issues(&[1, 2, 3]);
        let deps = make_deps(&[(1, &[]), (2, &[1]), (3, &[2])]);
        assert!(validate_dag(&issues, &deps).is_ok());
    }

    #[test]
    fn validate_dag_valid_diamond() {
        // A(1) → {B(2), C(3)} → D(4)
        let issues = make_issues(&[1, 2, 3, 4]);
        let deps = make_deps(&[(1, &[]), (2, &[1]), (3, &[1]), (4, &[2, 3])]);
        assert!(validate_dag(&issues, &deps).is_ok());
    }

    #[test]
    fn validate_dag_missing_upstream() {
        // Issue 2 depends on issue 99 which is not in the plan.
        let issues = make_issues(&[1, 2]);
        let deps = make_deps(&[(1, &[]), (2, &[99])]);
        let err = validate_dag(&issues, &deps).unwrap_err();
        assert!(
            err.contains("99"),
            "error should mention the missing issue: {err}"
        );
        assert!(
            err.contains("2"),
            "error should mention the dependent issue: {err}"
        );
    }

    #[test]
    fn validate_dag_self_cycle() {
        // Issue 1 depends on itself.
        let issues = make_issues(&[1]);
        let deps = make_deps(&[(1, &[1])]);
        let err = validate_dag(&issues, &deps).unwrap_err();
        assert!(err.contains("cycle"), "error should mention cycle: {err}");
        assert!(err.contains('1'), "error should mention issue 1: {err}");
    }

    #[test]
    fn validate_dag_two_node_cycle() {
        // 1 depends on 2 and 2 depends on 1.
        let issues = make_issues(&[1, 2]);
        let deps = make_deps(&[(1, &[2]), (2, &[1])]);
        let err = validate_dag(&issues, &deps).unwrap_err();
        assert!(err.contains("cycle"), "error should mention cycle: {err}");
        assert!(
            err.contains('1') && err.contains('2'),
            "error should mention both issues: {err}"
        );
    }

    #[test]
    fn validate_dag_multi_component_cycle() {
        // 3-node cycle {1,2,3} isolated from valid node 4.
        let issues = make_issues(&[1, 2, 3, 4]);
        let deps = make_deps(&[(1, &[3]), (2, &[1]), (3, &[2]), (4, &[])]);
        let err = validate_dag(&issues, &deps).unwrap_err();
        assert!(err.contains("cycle"), "error should mention cycle: {err}");
        // Issues 1, 2, 3 should all appear in the error
        assert!(err.contains('1') && err.contains('2') && err.contains('3'));
        // Issue 4 is valid and should not be in the cycle error
        // (4 is not in cycle_nodes because its in-degree stays 0)
    }

    #[test]
    fn validate_dag_missing_plus_cycle() {
        // Issue 1 depends on missing 99, and issues 2 and 3 form a cycle.
        let issues = make_issues(&[1, 2, 3]);
        let deps = make_deps(&[(1, &[99]), (2, &[3]), (3, &[2])]);
        let err = validate_dag(&issues, &deps).unwrap_err();
        assert!(
            err.contains("99"),
            "error should mention missing dep 99: {err}"
        );
        assert!(err.contains("cycle"), "error should mention cycle: {err}");
    }

    #[test]
    fn validate_dag_skipped_dep_excluded() {
        // Issue 2 depends on issue 10, but 10 is skipped (not in all_task_issues).
        // When the caller pre-filters skipped deps from the dep list, no error.
        let issues = make_issues(&[1, 2]);
        // Simulates the caller filtering out dep 10 (a skipped issue) before calling validate_dag.
        let deps = make_deps(&[(1, &[]), (2, &[])]);
        assert!(validate_dag(&issues, &deps).is_ok());
    }

    fn make_tasks(pairs: &[(u64, &[u64])]) -> Vec<harness_core::prompts::SprintTask> {
        pairs
            .iter()
            .map(|&(issue, deps)| harness_core::prompts::SprintTask {
                issue,
                depends_on: deps.to_vec(),
            })
            .collect()
    }

    #[test]
    fn normalize_sprint_tasks_drops_completed_missing_upstream() {
        let normalized = normalize_sprint_tasks(
            make_tasks(&[(1, &[]), (2, &[99])]),
            &std::collections::HashSet::new(),
            &make_issues(&[99]),
        );

        assert!(normalized.skipped_tasks.is_empty());
        assert_eq!(normalized.dropped_completed_edges, vec![(2, 99)]);
        assert_eq!(normalized.tasks, make_tasks(&[(1, &[]), (2, &[])]));
    }

    #[test]
    fn normalize_sprint_tasks_skips_unresolved_missing_upstream() {
        let normalized = normalize_sprint_tasks(
            make_tasks(&[(1, &[]), (2, &[99])]),
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        );

        assert!(normalized.dropped_completed_edges.is_empty());
        assert_eq!(normalized.tasks, make_tasks(&[(1, &[])]));
        assert_eq!(
            normalized.skipped_tasks,
            vec![NormalizedTaskSkip {
                issue: 2,
                upstream: 99,
                reason: "upstream issue 99 is outside the sprint plan and not completed"
                    .to_string(),
            }]
        );
    }

    #[test]
    fn normalize_sprint_tasks_handles_mixed_valid_and_invalid_edges() {
        let normalized = normalize_sprint_tasks(
            make_tasks(&[(1, &[]), (2, &[1]), (3, &[7]), (4, &[99]), (5, &[4])]),
            &std::collections::HashSet::new(),
            &make_issues(&[7]),
        );

        assert_eq!(normalized.dropped_completed_edges, vec![(3, 7)]);
        assert_eq!(
            normalized.tasks,
            make_tasks(&[(1, &[]), (2, &[1]), (3, &[])])
        );
        assert_eq!(
            normalized.skipped_tasks,
            vec![
                NormalizedTaskSkip {
                    issue: 4,
                    upstream: 99,
                    reason: "upstream issue 99 is outside the sprint plan and not completed"
                        .to_string(),
                },
                NormalizedTaskSkip {
                    issue: 5,
                    upstream: 4,
                    reason:
                        "upstream issue 4 was removed from this sprint during intake normalization"
                            .to_string(),
                },
            ]
        );
    }

    #[test]
    fn ready_issues_returns_root_when_dependency_not_completed() {
        let issues = make_issues(&[41, 42]);
        let deps = make_deps(&[(41, &[]), (42, &[41])]);
        let ready = ready_issues(
            &issues,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
            &std::collections::HashMap::new(),
            &deps,
        );
        assert_eq!(ready, vec![41]);
    }

    #[test]
    fn ready_issues_excludes_dependent_until_upstream_completed() {
        let issues = make_issues(&[41, 42]);
        let deps = make_deps(&[(41, &[]), (42, &[41])]);
        let completed = make_issues(&[41]);
        let ready = ready_issues(
            &issues,
            &completed,
            &std::collections::HashSet::new(),
            &std::collections::HashMap::new(),
            &deps,
        );
        assert_eq!(ready, vec![42]);
    }

    #[test]
    fn cancelled_tasks_do_not_count_as_completed_dependencies() {
        assert!(!status_unblocks_dependents(&TaskStatus::Cancelled));
    }

    #[test]
    fn done_and_failed_tasks_still_count_as_completed_dependencies() {
        assert!(status_unblocks_dependents(&TaskStatus::Done));
        assert!(status_unblocks_dependents(&TaskStatus::Failed));
    }

    #[tokio::test]
    async fn completed_issue_numbers_for_project_uses_repo_and_latest_status() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project_root = std::path::PathBuf::from("/projects/alpha");
        let other_project_root = std::path::PathBuf::from("/projects/beta");
        let repo = "owner/current";

        for (id, issue, status, root, task_repo, created_at) in [
            (
                "done",
                41_u64,
                TaskStatus::Done,
                &project_root,
                repo,
                Some("2026-04-22T09:00:00Z"),
            ),
            (
                "failed",
                42_u64,
                TaskStatus::Failed,
                &project_root,
                repo,
                Some("2026-04-22T09:05:00Z"),
            ),
            (
                "cancelled",
                43_u64,
                TaskStatus::Cancelled,
                &project_root,
                repo,
                Some("2026-04-22T09:10:00Z"),
            ),
            (
                "other-project",
                44_u64,
                TaskStatus::Done,
                &other_project_root,
                repo,
                Some("2026-04-22T09:15:00Z"),
            ),
            (
                "other-repo",
                45_u64,
                TaskStatus::Done,
                &project_root,
                "owner/other",
                Some("2026-04-22T09:20:00Z"),
            ),
            (
                "done-old-attempt",
                46_u64,
                TaskStatus::Done,
                &project_root,
                repo,
                Some("2026-04-22T09:25:00Z"),
            ),
            (
                "retry-latest",
                46_u64,
                TaskStatus::Implementing,
                &project_root,
                repo,
                Some("2026-04-22T09:30:00Z"),
            ),
        ] {
            let mut task =
                crate::task_runner::TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            task.repo = Some(task_repo.to_string());
            task.external_id = Some(format!("issue:{issue}"));
            task.created_at = created_at.map(str::to_string);
            store.insert(&task).await;
        }

        let completed = completed_issue_numbers_for_project(
            store.as_ref(),
            &project_root.to_string_lossy(),
            Some(repo),
        )
        .await;

        assert_eq!(completed, make_issues(&[41, 42]));
        Ok(())
    }

    #[test]
    fn latest_unblocking_issue_numbers_prefers_latest_issue_id_format() {
        let completed = latest_unblocking_issue_numbers(std::collections::HashMap::from([
            (
                "42".to_string(),
                (Some("2026-04-22T09:00:00Z".to_string()), TaskStatus::Done),
            ),
            (
                "issue:42".to_string(),
                (
                    Some("2026-04-22T09:05:00Z".to_string()),
                    TaskStatus::Implementing,
                ),
            ),
            (
                "issue:43".to_string(),
                (Some("2026-04-22T09:10:00Z".to_string()), TaskStatus::Done),
            ),
        ]));

        assert_eq!(completed, make_issues(&[43]));
    }

    #[test]
    fn workflow_ready_to_merge_and_terminals_unblock_dependents() {
        use harness_workflow::issue_lifecycle::IssueLifecycleState;

        assert!(workflow_state_unblocks_dependents(
            IssueLifecycleState::ReadyToMerge
        ));
        assert!(workflow_state_unblocks_dependents(
            IssueLifecycleState::Done
        ));
        assert!(workflow_state_unblocks_dependents(
            IssueLifecycleState::Failed
        ));
        assert!(!workflow_state_unblocks_dependents(
            IssueLifecycleState::Cancelled
        ));
        assert!(!workflow_state_unblocks_dependents(
            IssueLifecycleState::AddressingFeedback
        ));
    }

    #[test]
    fn sprint_issue_outcome_labels_are_stable() {
        assert_eq!(
            SprintIssueOutcome::UnblocksDependents.as_str(),
            "unblocks_dependents"
        );
        assert_eq!(SprintIssueOutcome::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn sprint_available_slots_clamps_to_global_headroom() {
        let diag = crate::task_queue::QueueDiagnostics {
            global_running: 3,
            global_queued: 0,
            global_limit: 4,
            project_running: 1,
            project_waiting_for_project: 0,
            project_awaiting_global: 0,
            project_limit: 8,
        };
        assert_eq!(sprint_available_slots(&diag), 1);
    }

    #[test]
    fn transient_retry_delay_grows_exponentially_and_caps() {
        let base = Duration::from_secs(15);
        let max = Duration::from_secs(120);
        assert_eq!(transient_retry_delay(base, max, 0), Duration::from_secs(15));
        assert_eq!(transient_retry_delay(base, max, 1), Duration::from_secs(30));
        assert_eq!(transient_retry_delay(base, max, 2), Duration::from_secs(60));
        assert_eq!(
            transient_retry_delay(base, max, 3),
            Duration::from_secs(120)
        );
        assert_eq!(
            transient_retry_delay(base, max, 5),
            Duration::from_secs(120)
        );
    }

    // --- existing tests ---

    #[test]
    fn build_prompt_from_issue_formats_correctly() {
        let issue = IncomingIssue {
            source: "github".to_string(),
            external_id: "42".to_string(),
            identifier: "#42".to_string(),
            title: "Fix login bug".to_string(),
            description: Some("Users cannot log in after password reset.".to_string()),
            repo: Some("owner/repo".to_string()),
            url: Some("https://github.com/owner/repo/issues/42".to_string()),
            priority: None,
            labels: vec!["harness".to_string()],
            created_at: None,
            project_root: None,
        };

        let prompt = build_prompt_from_issue(&issue);
        assert!(prompt.contains("github issue #42: Fix login bug"));
        assert!(prompt.contains("https://github.com/owner/repo/issues/42"));
        assert!(prompt.contains("Users cannot log in after password reset."));
        assert!(prompt.contains("unattended session"));
    }

    #[test]
    fn build_prompt_uses_na_when_url_missing() {
        let issue = IncomingIssue {
            source: "github".to_string(),
            external_id: "1".to_string(),
            identifier: "#1".to_string(),
            title: "Task".to_string(),
            description: None,
            repo: None,
            url: None,
            priority: None,
            labels: vec![],
            created_at: None,
            project_root: None,
        };

        let prompt = build_prompt_from_issue(&issue);
        assert!(prompt.contains("URL: N/A"));
        assert!(prompt.contains("No description."));
    }

    #[test]
    fn build_orchestrator_with_no_config_returns_empty_orchestrator() {
        let config = harness_core::config::intake::IntakeConfig::default();
        let orchestrator = build_orchestrator(&config, None, None, vec![], None);
        assert!(orchestrator.sources.is_empty());
    }

    #[test]
    fn build_orchestrator_with_disabled_github_returns_empty() {
        let mut config = harness_core::config::intake::IntakeConfig::default();
        config.github = Some(harness_core::config::intake::GitHubIntakeConfig {
            enabled: false,
            repo: "owner/repo".to_string(),
            ..Default::default()
        });
        let orchestrator = build_orchestrator(&config, None, None, vec![], None);
        assert!(orchestrator.sources.is_empty());
    }

    #[test]
    fn build_orchestrator_with_enabled_github_registers_source() {
        let mut config = harness_core::config::intake::IntakeConfig::default();
        config.github = Some(harness_core::config::intake::GitHubIntakeConfig {
            enabled: true,
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            poll_interval_secs: 60,
            sprint_timeout_secs: 7200,
            retry_backoff_base_secs: 10,
            retry_backoff_max_secs: 90,
            ..Default::default()
        });
        let orchestrator = build_orchestrator(&config, None, None, vec![], None);
        assert_eq!(orchestrator.sources.len(), 1);
        assert_eq!(orchestrator.sources[0].name(), "github");
        assert_eq!(orchestrator.poll_interval, Duration::from_secs(60));
        assert_eq!(orchestrator.sprint_timeout, Duration::from_secs(7200));
        assert_eq!(orchestrator.retry_backoff_base, Duration::from_secs(10));
        assert_eq!(orchestrator.retry_backoff_max, Duration::from_secs(90));
    }

    #[test]
    fn build_orchestrator_skips_feishu_when_verification_token_missing() {
        let mut config = harness_core::config::intake::IntakeConfig::default();
        config.feishu = Some(harness_core::config::intake::FeishuIntakeConfig {
            enabled: true,
            app_id: None,
            app_secret: None,
            verification_token: None,
            trigger_keyword: "harness".to_string(),
            default_repo: None,
        });

        let orchestrator = build_orchestrator(&config, None, None, vec![], None);
        assert!(orchestrator.sources.is_empty());
    }
}
