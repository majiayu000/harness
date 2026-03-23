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
            loop {
                tokio::time::sleep(self.poll_interval).await;
                self.poll_tick(&state).await;
            }
        });
    }

    async fn poll_tick(&self, state: &Arc<AppState>) {
        // 1. Collect all new issues from all sources, grouped by repo.
        #[allow(clippy::type_complexity)]
        let mut by_repo: std::collections::HashMap<
            String,
            Vec<(Arc<dyn IntakeSource>, IncomingIssue)>,
        > = std::collections::HashMap::new();

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
            let skip_id = TaskId(format!("skip-{ext_id}"));
            if let Err(e) = source.mark_dispatched(&ext_id, &skip_id).await {
                tracing::warn!(external_id = %ext_id, "intake: failed to mark skipped: {e}");
            }
            tracing::info!(repo, external_id = %ext_id, reason = %skip.reason, "intake: issue skipped");
        }
    }

    // 3. DAG slot-filling execution.
    // Track: which issues are completed, which are running, which are waiting.
    let mut completed: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut running: std::collections::HashMap<u64, TaskId> = std::collections::HashMap::new();
    let all_task_issues: std::collections::HashSet<u64> =
        plan.tasks.iter().map(|t| t.issue).collect();
    let deps: std::collections::HashMap<u64, Vec<u64>> = plan
        .tasks
        .iter()
        .map(|t| (t.issue, t.depends_on.clone()))
        .collect();

    let max_slots = 4usize; // matches max_concurrent in config
    let start = tokio::time::Instant::now();

    loop {
        // All done?
        if completed.len() + plan.skip.len() >= all_task_issues.len() + plan.skip.len()
            && running.is_empty()
        {
            break;
        }
        if completed.len() >= all_task_issues.len() {
            break;
        }
        if start.elapsed() > TASK_TIMEOUT {
            tracing::warn!(repo, "intake: DAG execution timed out");
            break;
        }

        // Find ready tasks: not started, not completed, all deps satisfied.
        let ready: Vec<u64> = all_task_issues
            .iter()
            .filter(|&&issue| {
                !completed.contains(&issue)
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

            let placeholder_id = TaskId(format!("pending-{ext_id}"));
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
                source: Some("github".to_string()),
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
                Err(e) => {
                    tracing::error!(repo, external_id = %ext_id, "intake: failed to spawn: {e:?}");
                    source.unmark_dispatched(&ext_id).await;
                    completed.insert(issue_num);
                }
            }
        }

        if running.is_empty() {
            break;
        }

        // Wait for any running task to complete.
        tokio::time::sleep(TASK_POLL_INTERVAL).await;
        let mut newly_done = Vec::new();
        for (&issue_num, task_id) in &running {
            if let Some(task) = state.core.tasks.get(task_id) {
                if matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
                    tracing::info!(
                        repo,
                        external_id = issue_num,
                        task_id = %task_id,
                        status = task.status.as_ref(),
                        "intake: task finished"
                    );
                    newly_done.push(issue_num);
                }
            }
        }
        for issue_num in newly_done {
            running.remove(&issue_num);
            completed.insert(issue_num);
        }
    }

    tracing::info!(
        repo,
        completed = completed.len(),
        elapsed_secs = start.elapsed().as_secs(),
        "intake: repo sprint complete"
    );
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
        if !matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
            continue;
        }
        if matches!(task.status, TaskStatus::Failed) {
            tracing::error!(task_id = %task_id, error = ?task.error, "intake: task failed");
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
pub fn build_orchestrator(
    config: &harness_core::IntakeConfig,
    data_dir: Option<&std::path::Path>,
    feishu_intake: Option<Arc<feishu::FeishuIntake>>,
) -> IntakeOrchestrator {
    let mut sources: Vec<Arc<dyn IntakeSource>> = Vec::new();
    let mut poll_interval = Duration::from_secs(30);
    let mut planner_agent = None;

    if let Some(gh_config) = &config.github {
        if gh_config.enabled {
            poll_interval = Duration::from_secs(gh_config.poll_interval_secs);
            planner_agent = gh_config.planner_agent.clone();
            for repo_cfg in gh_config.effective_repos() {
                tracing::info!(
                    repo = %repo_cfg.repo,
                    label = %repo_cfg.label,
                    "intake: GitHub Issues poller registered"
                );
                let poller = github_issues::GitHubIssuesPoller::new(&repo_cfg, data_dir);
                sources.push(Arc::new(poller));
            }
        }
    }

    if let Some(feishu_config) = &config.feishu {
        if feishu_config.enabled {
            let intake = feishu_intake
                .unwrap_or_else(|| Arc::new(feishu::FeishuIntake::new(feishu_config.clone())));
            sources.push(intake);
            tracing::info!(
                trigger_keyword = %feishu_config.trigger_keyword,
                "intake: Feishu bot registered in orchestrator"
            );
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
        let config = harness_core::IntakeConfig::default();
        let orchestrator = build_orchestrator(&config, None, None);
        assert!(orchestrator.sources.is_empty());
    }

    #[test]
    fn build_orchestrator_with_disabled_github_returns_empty() {
        let mut config = harness_core::IntakeConfig::default();
        config.github = Some(harness_core::GitHubIntakeConfig {
            enabled: false,
            repo: "owner/repo".to_string(),
            ..Default::default()
        });
        let orchestrator = build_orchestrator(&config, None, None);
        assert!(orchestrator.sources.is_empty());
    }

    #[test]
    fn build_orchestrator_with_enabled_github_registers_source() {
        let mut config = harness_core::IntakeConfig::default();
        config.github = Some(harness_core::GitHubIntakeConfig {
            enabled: true,
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            poll_interval_secs: 60,
            ..Default::default()
        });
        let orchestrator = build_orchestrator(&config, None, None);
        assert_eq!(orchestrator.sources.len(), 1);
        assert_eq!(orchestrator.sources[0].name(), "github");
        assert_eq!(orchestrator.poll_interval, Duration::from_secs(60));
    }
}
