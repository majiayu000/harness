use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Deserialize;

use super::{IncomingIssue, IntakeSource, TaskCompletionResult};
use crate::task_runner::{TaskId, TaskStatus};

pub struct GitHubIssuesPoller {
    repo: String,
    label: String,
    dispatched: DashMap<String, TaskId>,
}

impl GitHubIssuesPoller {
    pub fn new(config: &harness_core::GitHubIntakeConfig) -> Self {
        Self {
            repo: config.repo.clone(),
            label: config.label.clone(),
            dispatched: DashMap::new(),
        }
    }
}

/// Raw GitHub issue fields returned by `gh issue list --json`.
#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    body: Option<String>,
    url: String,
    labels: Vec<GhLabel>,
    #[serde(rename = "createdAt")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

/// Parse the JSON output of `gh issue list --json number,title,body,url,labels,createdAt`
/// into a vec of `IncomingIssue`, filtering out already-dispatched issues.
fn parse_gh_output(
    json: &[u8],
    repo: &str,
    dispatched: &DashMap<String, TaskId>,
) -> anyhow::Result<Vec<IncomingIssue>> {
    let issues: Vec<GhIssue> = serde_json::from_slice(json)?;
    let result = issues
        .into_iter()
        .filter(|issue| !dispatched.contains_key(&issue.number.to_string()))
        .map(|issue| IncomingIssue {
            source: "github".to_string(),
            external_id: issue.number.to_string(),
            identifier: format!("#{}", issue.number),
            title: issue.title,
            description: issue.body,
            repo: Some(repo.to_string()),
            url: Some(issue.url),
            priority: None,
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            created_at: issue.created_at,
        })
        .collect();
    Ok(result)
}

#[async_trait]
impl IntakeSource for GitHubIssuesPoller {
    fn name(&self) -> &str {
        "github"
    }

    async fn poll(&self) -> anyhow::Result<Vec<IncomingIssue>> {
        let output = tokio::process::Command::new("gh")
            .args([
                "issue",
                "list",
                "--repo",
                &self.repo,
                "--label",
                &self.label,
                "--state",
                "open",
                "--json",
                "number,title,body,url,labels,createdAt",
            ])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh issue list failed: {stderr}");
        }

        parse_gh_output(&output.stdout, &self.repo, &self.dispatched)
    }

    async fn mark_dispatched(&self, external_id: &str, task_id: &TaskId) -> anyhow::Result<()> {
        self.dispatched
            .insert(external_id.to_string(), task_id.clone());

        let comment = format!("Harness task `{}` created. Working on it...", task_id.0);
        if let Err(e) = tokio::process::Command::new("gh")
            .args([
                "issue",
                "comment",
                external_id,
                "--repo",
                &self.repo,
                "--body",
                &comment,
            ])
            .output()
            .await
        {
            tracing::warn!(
                repo = %self.repo,
                issue = %external_id,
                "failed to post task-created comment: {e}"
            );
        }

        Ok(())
    }

    async fn on_task_complete(
        &self,
        external_id: &str,
        result: &TaskCompletionResult,
    ) -> anyhow::Result<()> {
        let body_and_context = match result.status {
            TaskStatus::Done => {
                let body = match &result.pr_url {
                    Some(pr_url) => format!("Task complete. PR: {pr_url}\n\n{}", result.summary),
                    None => format!("Task complete.\n\n{}", result.summary),
                };
                Some((body, "completion"))
            }
            TaskStatus::Failed => {
                let body = format!(
                    "Task failed: {}",
                    result.error.as_deref().unwrap_or("unknown error")
                );
                Some((body, "failure"))
            }
            _ => None,
        };

        if let Some((body, context)) = body_and_context {
            if let Err(e) = tokio::process::Command::new("gh")
                .args([
                    "issue",
                    "comment",
                    external_id,
                    "--repo",
                    &self.repo,
                    "--body",
                    &body,
                ])
                .output()
                .await
            {
                tracing::warn!(
                    repo = %self.repo,
                    issue = %external_id,
                    "failed to post {context} comment: {e}"
                );
            }
            self.dispatched.remove(external_id);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dispatched(ids: &[&str]) -> DashMap<String, TaskId> {
        let map = DashMap::new();
        for id in ids {
            map.insert(id.to_string(), TaskId(format!("task-{id}")));
        }
        map
    }

    #[test]
    fn parse_gh_output_converts_issues_to_incoming() {
        let json = br#"[
            {
                "number": 42,
                "title": "Fix login bug",
                "body": "Users cannot log in after password reset.",
                "url": "https://github.com/owner/repo/issues/42",
                "labels": [{"name": "harness"}, {"name": "bug"}],
                "createdAt": "2026-03-01T10:00:00Z"
            }
        ]"#;

        let dispatched = DashMap::new();
        let issues = parse_gh_output(json, "owner/repo", &dispatched).unwrap();

        assert_eq!(issues.len(), 1);
        let issue = &issues[0];
        assert_eq!(issue.source, "github");
        assert_eq!(issue.external_id, "42");
        assert_eq!(issue.identifier, "#42");
        assert_eq!(issue.title, "Fix login bug");
        assert_eq!(
            issue.description.as_deref(),
            Some("Users cannot log in after password reset.")
        );
        assert_eq!(issue.repo.as_deref(), Some("owner/repo"));
        assert_eq!(
            issue.url.as_deref(),
            Some("https://github.com/owner/repo/issues/42")
        );
        assert_eq!(issue.labels, vec!["harness", "bug"]);
    }

    #[test]
    fn parse_gh_output_filters_dispatched_issues() {
        let json = br#"[
            {"number": 1, "title": "A", "body": null, "url": "u1", "labels": [], "createdAt": null},
            {"number": 2, "title": "B", "body": null, "url": "u2", "labels": [], "createdAt": null},
            {"number": 3, "title": "C", "body": null, "url": "u3", "labels": [], "createdAt": null}
        ]"#;

        // Issues 1 and 2 already dispatched
        let dispatched = make_dispatched(&["1", "2"]);
        let issues = parse_gh_output(json, "owner/repo", &dispatched).unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_id, "3");
    }

    #[test]
    fn parse_gh_output_empty_array() {
        let json = b"[]";
        let dispatched = DashMap::new();
        let issues = parse_gh_output(json, "owner/repo", &dispatched).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn parse_gh_output_invalid_json_returns_error() {
        let json = b"not valid json";
        let dispatched = DashMap::new();
        let result = parse_gh_output(json, "owner/repo", &dispatched);
        assert!(result.is_err());
    }

    #[test]
    fn parse_gh_output_null_body_becomes_none_description() {
        let json = br#"[
            {"number": 5, "title": "No body", "body": null, "url": "u", "labels": [], "createdAt": null}
        ]"#;
        let dispatched = DashMap::new();
        let issues = parse_gh_output(json, "owner/repo", &dispatched).unwrap();
        assert_eq!(issues[0].description, None);
    }

    #[test]
    fn github_issues_poller_name_is_github() {
        let config = harness_core::GitHubIntakeConfig {
            enabled: true,
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            poll_interval_secs: 30,
        };
        let poller = GitHubIssuesPoller::new(&config);
        assert_eq!(poller.name(), "github");
    }
}
