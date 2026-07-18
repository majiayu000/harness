use async_trait::async_trait;
use chrono::{DateTime, Utc};
use harness_core::config::isolation::IsolationTrustClass;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::http::AppState;
use crate::workflow_runtime_submission::{
    runtime_models::{TaskFailureKind, TaskId, TaskStatus},
    CreateTaskRequest, MAX_TASK_PRIORITY,
};

pub mod binding;
mod declarative_routing;
mod direct_dispatch;
pub mod feishu;
mod github_coverage_gate;
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
    /// Trust class derived from source metadata. Non-GitHub/internal sources default to trusted.
    #[serde(default)]
    pub author_trust_class: IsolationTrustClass,
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

#[derive(Clone)]
pub struct IntakeSourceRegistration {
    pub source: Arc<dyn IntakeSource>,
    pub repo_hint: Option<String>,
}

impl IntakeSourceRegistration {
    pub fn new(source: Arc<dyn IntakeSource>, repo_hint: Option<String>) -> Self {
        Self { source, repo_hint }
    }
}

/// Orchestrates all registered intake sources with direct workflow-runtime dispatch.
///
/// GitHub issues pass through a server-owned coverage gate and are submitted
/// directly into the `github_issue_pr` runtime workflow when uncovered.
pub struct IntakeOrchestrator {
    sources: Vec<IntakeSourceRegistration>,
    poll_interval: Duration,
}

impl IntakeOrchestrator {
    pub fn new(
        sources: Vec<IntakeSourceRegistration>,
        poll_interval: Duration,
        _planner_agent: Option<String>,
        _sprint_timeout: Duration,
        _retry_backoff_base: Duration,
        _retry_backoff_max: Duration,
    ) -> Self {
        Self {
            sources,
            poll_interval,
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

        for registration in &self.sources {
            let source = &registration.source;
            if source.name() == "github" {
                if let Some(repo) = registration.repo_hint.as_deref() {
                    state.intake.record_github_token_dispatch(
                        repo,
                        crate::http::GitHubTokenDispatchMetric::ServerGithubPoll,
                    );
                }
            }
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

        // 2. Dispatch uncovered issues for each repo in parallel.
        let mut handles = Vec::new();
        for (repo, issues) in by_repo {
            let project_root = issues
                .first()
                .and_then(|(_, i)| i.project_root.clone())
                .unwrap_or_else(|| state.core.project_root.clone());
            let project_id = tokio::fs::canonicalize(&project_root)
                .await
                .unwrap_or_else(|_| state.core.project_root.clone())
                .to_string_lossy()
                .into_owned();
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
            let mut uncovered_issues = Vec::new();
            for (source, issue) in issues {
                // GH-1656: declarative intake routing runs first and is
                // exclusive — a matched issue never reaches the github path.
                if declarative_routing::route_declarative_intake(
                    state,
                    &project_root,
                    &source,
                    &issue,
                )
                .await
                {
                    continue;
                }
                let Some(issue_number) = github_direct_issue_number(source.name(), &issue) else {
                    enqueue_fallback_intake_issue(
                        state,
                        Arc::clone(&source),
                        issue,
                        project_root.clone(),
                    )
                    .await;
                    continue;
                };
                let fact_hash = match github_coverage_gate::record_issue_remote_fact_snapshot(
                    state.core.workflow_runtime_store.as_deref(),
                    &repo,
                    issue_number,
                    &issue,
                )
                .await
                {
                    Ok(fact_hash) => fact_hash,
                    Err(error) => {
                        tracing::warn!(
                            repo,
                            issue = issue_number,
                            "intake: failed to record GitHub issue remote fact, skipping this tick: {error}"
                        );
                        continue;
                    }
                };
                match github_coverage_gate::check_github_issue_coverage(
                    state.core.issue_workflow_store.as_deref(),
                    state.core.workflow_runtime_store.as_deref(),
                    &project_id,
                    &repo,
                    issue_number,
                )
                .await
                {
                    Ok(github_coverage_gate::GitHubIssueCoverage::Covered {
                        source: coverage_source,
                        state: workflow_state,
                    }) => {
                        state.intake.record_github_token_dispatch(
                            &repo,
                            crate::http::GitHubTokenDispatchMetric::AgentSkippedCoveredIssue,
                        );
                        tracing::info!(
                            repo,
                            issue = issue_number,
                            coverage_source,
                            workflow_state = %workflow_state,
                            "intake: skipping covered GitHub issue before runtime dispatch"
                        );
                    }
                    Ok(github_coverage_gate::GitHubIssueCoverage::Uncovered) => {
                        uncovered_issues.push(direct_dispatch::DirectDispatchIssue {
                            source,
                            issue,
                            fact_hash,
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            repo,
                            issue = issue_number,
                            "intake: failed to check GitHub issue coverage, skipping this tick: {error}"
                        );
                    }
                }
            }
            if uncovered_issues.is_empty() {
                tracing::debug!(
                    repo,
                    "intake: all GitHub issues covered before runtime dispatch"
                );
                record_intake_idle(state, &project_id, Some(&repo), 0).await;
                continue;
            }
            let state = Arc::clone(state);
            handles.push(tokio::spawn(async move {
                direct_dispatch::run_direct_issue_dispatch(
                    &state,
                    &repo,
                    uncovered_issues,
                    project_root,
                    project_id,
                )
                .await;
            }));
        }

        // Wait for all repo dispatch tasks to finish.
        for handle in handles {
            if let Err(e) = handle.await {
                tracing::error!("intake: repo dispatch task panicked: {e}");
            }
        }
    }
}

async fn record_intake_idle(
    state: &Arc<AppState>,
    project_id: &str,
    repo: Option<&str>,
    dispatched: usize,
) {
    if let Some(workflows) = state.core.project_workflow_store.as_ref() {
        if let Err(error) = workflows.record_idle(project_id, repo).await {
            tracing::warn!(
                repo = repo.unwrap_or("<none>"),
                dispatched,
                "intake: failed to mark project workflow idle after poll tick: {error}"
            );
        }
    }
}

fn github_direct_issue_number(source_name: &str, issue: &IncomingIssue) -> Option<u64> {
    (source_name == "github")
        .then(|| issue.external_id.parse::<u64>().ok())
        .flatten()
}

async fn enqueue_fallback_intake_issue(
    state: &Arc<AppState>,
    source: Arc<dyn IntakeSource>,
    issue: IncomingIssue,
    default_project_root: std::path::PathBuf,
) {
    let external_id = issue.external_id.clone();
    let req = fallback_intake_task_request(&issue, source.name(), default_project_root);
    match crate::http::task_routes::enqueue_task_background(Arc::clone(state), req, None).await {
        Ok(task_id) => {
            if let Err(error) = source.mark_dispatched(&external_id, &task_id).await {
                tracing::warn!(
                    source = source.name(),
                    external_id,
                    task_id = %task_id,
                    "intake: fallback task was enqueued but mark_dispatched failed: {error}"
                );
            }
        }
        Err(error) => {
            source.unmark_dispatched(&external_id).await;
            tracing::error!(
                source = source.name(),
                external_id,
                "intake: fallback task enqueue failed: {error}"
            );
        }
    }
}

fn fallback_intake_task_request(
    issue: &IncomingIssue,
    source_name: &str,
    default_project_root: std::path::PathBuf,
) -> CreateTaskRequest {
    let priority = issue
        .priority
        .and_then(|priority| u8::try_from(priority).ok())
        .filter(|priority| *priority <= MAX_TASK_PRIORITY)
        .unwrap_or_default();
    CreateTaskRequest {
        prompt: Some(build_prompt_from_issue(issue)),
        project: Some(issue.project_root.clone().unwrap_or(default_project_root)),
        source: Some(source_name.to_string()),
        external_id: Some(issue.external_id.clone()),
        repo: issue.repo.clone(),
        labels: issue.labels.clone(),
        priority,
        ..Default::default()
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
    feishu_intake: Option<Arc<feishu::FeishuIntake>>,
    github_sources: Vec<IntakeSourceRegistration>,
) -> IntakeOrchestrator {
    let mut sources: Vec<IntakeSourceRegistration> = Vec::new();
    let mut poll_interval = Duration::from_secs(30);
    let mut planner_agent = None;
    let mut sprint_timeout = Duration::from_secs(3 * 60 * 60);
    let mut retry_backoff_base = Duration::from_secs(15);
    let mut retry_backoff_max = Duration::from_secs(120);

    if let Some(gh_config) = &config.github {
        if gh_config.enabled && gh_config.mode.poller_enabled() {
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
                tracing::info!("intake: no GitHub issue pollers registered");
            }
            sources.extend(github_sources);
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
                sources.push(IntakeSourceRegistration::new(intake, None));
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
    harness_core::prompts::intake::from_incoming_issue(
        &harness_core::prompts::intake::IncomingIssueView {
            source: &issue.source,
            identifier: &issue.identifier,
            title: &issue.title,
            url: issue.url.as_deref(),
            description: issue.description.as_deref(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming_issue(source: &str, external_id: &str) -> IncomingIssue {
        IncomingIssue {
            source: source.to_string(),
            external_id: external_id.to_string(),
            identifier: external_id.to_string(),
            title: "Investigate intake fallback".to_string(),
            description: Some("Handle the incoming item.".to_string()),
            repo: Some("owner/repo".to_string()),
            url: None,
            priority: Some(2),
            labels: vec!["inbox".to_string()],
            created_at: None,
            author_trust_class: harness_core::config::isolation::IsolationTrustClass::Trusted,
            project_root: None,
        }
    }

    #[test]
    fn github_direct_issue_number_only_accepts_numeric_github_issues() {
        assert_eq!(
            github_direct_issue_number("github", &incoming_issue("github", "42")),
            Some(42)
        );
        assert_eq!(
            github_direct_issue_number("github", &incoming_issue("github", "msg-42")),
            None
        );
        assert_eq!(
            github_direct_issue_number("feishu", &incoming_issue("feishu", "42")),
            None
        );
    }

    #[test]
    fn fallback_intake_task_request_preserves_source_identity() {
        let project_root = std::path::PathBuf::from("/tmp/project");
        let issue = incoming_issue("feishu", "msg-42");

        let req = fallback_intake_task_request(&issue, "feishu", project_root.clone());

        assert!(req
            .prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("feishu")));
        assert_eq!(req.project, Some(project_root));
        assert_eq!(req.source.as_deref(), Some("feishu"));
        assert_eq!(req.external_id.as_deref(), Some("msg-42"));
        assert_eq!(req.repo.as_deref(), Some("owner/repo"));
        assert_eq!(req.labels, vec!["inbox"]);
        assert_eq!(req.priority, 2);
        assert_eq!(req.issue, None);
    }
}
