use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::http::AppState;
use crate::task_runner::{TaskId, TaskStatus};

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
}

/// Timeout for waiting on a single task or round to complete (30 minutes).
const TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Polling interval when waiting for task completion.
const TASK_POLL_INTERVAL: Duration = Duration::from_secs(15);

impl IntakeOrchestrator {
    pub fn new(
        sources: Vec<Arc<dyn IntakeSource>>,
        poll_interval: Duration,
        planner_agent: Option<String>,
    ) -> Self {
        Self {
            sources,
            poll_interval,
            planner_agent,
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
            let state = Arc::clone(state);
            let planner_agent = self.planner_agent.clone();
            handles.push(tokio::spawn(async move {
                run_repo_sprint(&state, &repo, issues, planner_agent.as_deref()).await;
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

/// Run sprint planner + DAG-based slot-filling execution for a single repo.
async fn run_repo_sprint(
    state: &Arc<AppState>,
    repo: &str,
    issues: Vec<(Arc<dyn IntakeSource>, IncomingIssue)>,
    planner_agent: Option<&str>,
) {
    tracing::info!(
        repo,
        count = issues.len(),
        "intake: planning sprint for repo"
    );

    let project_root = issues
        .first()
        .and_then(|(_, i)| i.project_root.clone())
        .unwrap_or_else(|| state.core.project_root.clone());

    // 1. Run sprint planner.
    let issue_summary = build_issue_summary(&issues);
    let planner_prompt = harness_core::prompts::sprint_plan_prompt(&issue_summary);

    let planner_req = crate::task_runner::CreateTaskRequest {
        prompt: Some(planner_prompt),
        agent: planner_agent.map(String::from),
        project: Some(project_root.clone()),
        source: Some("sprint_planner".to_string()),
        ..Default::default()
    };

    let planner_task_id = match crate::http::task_routes::enqueue_task(state, planner_req).await {
        Ok(id) => {
            tracing::info!(repo, task_id = %id, "intake: sprint planner enqueued");
            id
        }
        Err(e) => {
            tracing::error!(repo, "intake: failed to enqueue sprint planner: {e:?}");
            return;
        }
    };

    let Some(planner_output) = poll_task_output(&state.core.tasks, &planner_task_id).await else {
        tracing::error!(repo, task_id = %planner_task_id, "intake: sprint planner failed");
        return;
    };

    let Some(plan) = harness_core::prompts::parse_sprint_plan(&planner_output) else {
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

    // 2. Mark skipped.
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

    // Build the dep map, filtering out any dep IDs that belong to skipped issues
    // so they are not flagged as missing upstreams.
    let skip_set: std::collections::HashSet<u64> = plan.skip.iter().map(|s| s.issue).collect();
    let deps: std::collections::HashMap<u64, Vec<u64>> = plan
        .tasks
        .iter()
        .map(|t| {
            let filtered: Vec<u64> = t
                .depends_on
                .iter()
                .copied()
                .filter(|dep| !skip_set.contains(dep))
                .collect();
            (t.issue, filtered)
        })
        .collect();

    // Validate DAG before starting execution.
    if let Err(e) = validate_dag(&all_task_issues, &deps) {
        tracing::error!(repo, error = %e, "intake: invalid sprint DAG — aborting");
        return;
    }

    let project_id = project_root.to_string_lossy().into_owned();
    let max_slots = state
        .concurrency
        .task_queue
        .effective_project_limit(&project_id);
    let start = tokio::time::Instant::now();

    'sprint: loop {
        // All done?
        if completed.len() + cancelled_sprint.len() >= all_task_issues.len() && running.is_empty() {
            break;
        }
        if completed.len() + cancelled_sprint.len() >= all_task_issues.len() {
            break;
        }
        if start.elapsed() > TASK_TIMEOUT {
            tracing::warn!(repo, "intake: DAG execution timed out");
            break;
        }

        // Find ready tasks: not started, not completed, not cancelled, all deps satisfied.
        let ready: Vec<u64> = all_task_issues
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
            .collect();

        // Fill available slots.
        let available = max_slots.saturating_sub(running.len());
        for &issue_num in ready.iter().take(available) {
            let ext_id = issue_num.to_string();
            let Some((source, issue)) = issue_map.get(&ext_id) else {
                tracing::warn!(repo, external_id = %ext_id, "intake: issue not found");
                completed.insert(issue_num);
                continue;
            };

            let placeholder_id = harness_core::types::TaskId(format!("pending-{ext_id}"));
            if let Err(e) = source.mark_dispatched(&ext_id, &placeholder_id).await {
                tracing::warn!(external_id = %ext_id, "intake: mark_dispatched failed: {e}");
                completed.insert(issue_num);
                continue;
            }

            let req = crate::task_runner::CreateTaskRequest {
                issue: issue.external_id.parse().ok(),
                project: issue
                    .project_root
                    .clone()
                    .or_else(|| Some(project_root.clone())),
                source: Some(source.name().to_string()),
                external_id: Some(issue.external_id.clone()),
                repo: Some(repo.to_string()),
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
                Err(e) => {
                    tracing::error!(repo, external_id = %ext_id, "intake: failed to spawn: {e:?}");
                    source.unmark_dispatched(&ext_id).await;
                    completed.insert(issue_num);
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
                tracing::error!(
                    repo,
                    stranded = ?pending,
                    "intake: DAG deadlock — tasks stranded with unresolvable deps"
                );
            }
            break;
        }

        // Wait for any running task to complete.
        tokio::time::sleep(TASK_POLL_INTERVAL).await;
        let mut newly_done = Vec::new();
        let mut cancelled = Vec::new();
        for (&issue_num, task_id) in &running {
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

/// Poll a task until terminal state, return its output.
async fn poll_task_output(
    store: &crate::task_runner::TaskStore,
    task_id: &TaskId,
) -> Option<String> {
    let start = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(TASK_POLL_INTERVAL).await;
        if start.elapsed() > TASK_TIMEOUT {
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
) -> IntakeOrchestrator {
    let mut sources: Vec<Arc<dyn IntakeSource>> = Vec::new();
    let mut poll_interval = Duration::from_secs(30);
    let mut planner_agent = None;

    if let Some(gh_config) = &config.github {
        if gh_config.enabled {
            poll_interval = Duration::from_secs(gh_config.poll_interval_secs);
            planner_agent = gh_config.planner_agent.clone();
            if github_sources.is_empty() {
                // Fallback: build fresh pollers when no shared instances are supplied
                // (e.g. during testing or when called without pre-built sources).
                for repo_cfg in gh_config.effective_repos() {
                    tracing::info!(
                        repo = %repo_cfg.repo,
                        label = %repo_cfg.label,
                        "intake: GitHub Issues poller registered (fallback)"
                    );
                    let poller = github_issues::GitHubIssuesPoller::new(&repo_cfg, data_dir);
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

    IntakeOrchestrator::new(sources, poll_interval, planner_agent)
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

    #[test]
    fn cancelled_tasks_do_not_count_as_completed_dependencies() {
        assert!(!status_unblocks_dependents(&TaskStatus::Cancelled));
    }

    #[test]
    fn done_and_failed_tasks_still_count_as_completed_dependencies() {
        assert!(status_unblocks_dependents(&TaskStatus::Done));
        assert!(status_unblocks_dependents(&TaskStatus::Failed));
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
        let orchestrator = build_orchestrator(&config, None, None, vec![]);
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
        let orchestrator = build_orchestrator(&config, None, None, vec![]);
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
            ..Default::default()
        });
        let orchestrator = build_orchestrator(&config, None, None, vec![]);
        assert_eq!(orchestrator.sources.len(), 1);
        assert_eq!(orchestrator.sources[0].name(), "github");
        assert_eq!(orchestrator.poll_interval, Duration::from_secs(60));
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

        let orchestrator = build_orchestrator(&config, None, None, vec![]);
        assert!(orchestrator.sources.is_empty());
    }
}
