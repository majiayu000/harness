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

/// Orchestrates all registered intake sources: polls at interval, dispatches issues as tasks.
pub struct IntakeOrchestrator {
    sources: Vec<Arc<dyn IntakeSource>>,
    poll_interval: Duration,
}

impl IntakeOrchestrator {
    pub fn new(sources: Vec<Arc<dyn IntakeSource>>, poll_interval: Duration) -> Self {
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
            loop {
                tokio::time::sleep(self.poll_interval).await;
                self.poll_tick(&state).await;
            }
        });
    }

    async fn poll_tick(&self, state: &Arc<AppState>) {
        for source in &self.sources {
            let issues = match source.poll().await {
                Ok(issues) => issues,
                Err(e) => {
                    tracing::error!(source = source.name(), "intake poll failed: {e}");
                    continue;
                }
            };

            for issue in issues {
                let prompt = build_prompt_from_issue(&issue);
                let req = crate::task_runner::CreateTaskRequest {
                    prompt: Some(prompt),
                    issue: issue.external_id.parse().ok(),
                    project: issue.repo.as_ref().map(|_| state.project_root.clone()),
                    ..Default::default()
                };

                // Mark dispatched first to prevent duplicate tasks if enqueue
                // succeeds but we crash before persisting the dispatched state.
                let placeholder_id = TaskId(format!("pending-{}", issue.external_id));
                if let Err(e) = source
                    .mark_dispatched(&issue.external_id, &placeholder_id)
                    .await
                {
                    tracing::warn!(
                        source = source.name(),
                        external_id = %issue.external_id,
                        "mark_dispatched failed: {e}"
                    );
                    continue;
                }

                match crate::http::task_routes::enqueue_task(state, req).await {
                    Ok(task_id) => {
                        tracing::info!(
                            source = source.name(),
                            external_id = %issue.external_id,
                            task_id = %task_id.0,
                            "intake: task dispatched"
                        );
                        // Update with the real task ID.
                        if let Err(e) = source.mark_dispatched(&issue.external_id, &task_id).await {
                            tracing::warn!(
                                source = source.name(),
                                external_id = %issue.external_id,
                                "mark_dispatched update failed: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            source = source.name(),
                            external_id = %issue.external_id,
                            "intake: failed to spawn task: {e:?}"
                        );
                        // Un-mark on enqueue failure so the issue is retried next poll.
                        source.unmark_dispatched(&issue.external_id).await;
                    }
                }
            }
        }
    }
}

/// Build an `IntakeOrchestrator` from config, registering all enabled sources.
pub fn build_orchestrator(
    config: &harness_core::IntakeConfig,
    data_dir: Option<&std::path::Path>,
) -> IntakeOrchestrator {
    let mut sources: Vec<Arc<dyn IntakeSource>> = Vec::new();
    let mut poll_interval = Duration::from_secs(30);

    if let Some(gh_config) = &config.github {
        if gh_config.enabled && !gh_config.repo.is_empty() {
            poll_interval = Duration::from_secs(gh_config.poll_interval_secs);
            let poller = github_issues::GitHubIssuesPoller::new(gh_config, data_dir);
            sources.push(Arc::new(poller));
            tracing::info!(
                repo = %gh_config.repo,
                label = %gh_config.label,
                poll_interval_secs = gh_config.poll_interval_secs,
                "intake: GitHub Issues poller registered"
            );
        }
    }

    if let Some(feishu_config) = &config.feishu {
        if feishu_config.enabled {
            let intake = feishu::FeishuIntake::new(feishu_config.clone());
            sources.push(Arc::new(intake));
            tracing::info!(
                trigger_keyword = %feishu_config.trigger_keyword,
                "intake: Feishu bot registered in orchestrator"
            );
        }
    }

    IntakeOrchestrator::new(sources, poll_interval)
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
        };

        let prompt = build_prompt_from_issue(&issue);
        assert!(prompt.contains("URL: N/A"));
        assert!(prompt.contains("No description."));
    }

    #[test]
    fn build_orchestrator_with_no_config_returns_empty_orchestrator() {
        let config = harness_core::IntakeConfig::default();
        let orchestrator = build_orchestrator(&config, None);
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
        let orchestrator = build_orchestrator(&config, None);
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
        });
        let orchestrator = build_orchestrator(&config, None);
        assert_eq!(orchestrator.sources.len(), 1);
        assert_eq!(orchestrator.sources[0].name(), "github");
        assert_eq!(orchestrator.poll_interval, Duration::from_secs(60));
    }
}
