use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{IncomingIssue, IntakeSource, TaskCompletionResult};
use crate::task_runner::TaskId;

pub struct GitHubIssuesPoller {
    repo: String,
    label: String,
    project_root: Option<PathBuf>,
    dispatched: DashMap<String, TaskId>,
    persist_path: Option<PathBuf>,
}

impl GitHubIssuesPoller {
    pub fn new(
        repo_config: &harness_core::config::intake::GitHubRepoConfig,
        data_dir: Option<&Path>,
    ) -> Self {
        let repo_slug = repo_config.repo.replace('/', "_");
        let persist_path = data_dir.map(|d| d.join(format!("github_dispatched_{repo_slug}.json")));
        let dispatched = Self::load_dispatched(persist_path.as_deref());
        Self {
            repo: repo_config.repo.clone(),
            label: repo_config.label.clone(),
            project_root: repo_config.project_root.as_ref().map(PathBuf::from),
            dispatched,
            persist_path,
        }
    }

    fn load_dispatched(path: Option<&Path>) -> DashMap<String, TaskId> {
        let Some(path) = path else {
            return DashMap::new();
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DashMap::new(),
            Err(e) => {
                tracing::warn!(
                    "failed to read dispatched state from {}: {e}",
                    path.display()
                );
                return DashMap::new();
            }
        };
        let map: HashMap<String, String> = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "failed to load dispatched state from {}: {e}",
                    path.display()
                );
                return DashMap::new();
            }
        };
        let dm = DashMap::new();
        for (k, v) in map {
            dm.insert(k, harness_core::types::TaskId(v));
        }
        dm
    }

    fn persist_dispatched(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("failed to create dispatched state directory: {e}");
                return;
            }
        }
        let map: HashMap<String, String> = self
            .dispatched
            .iter()
            .map(|e| (e.key().clone(), e.value().0.clone()))
            .collect();
        match serde_json::to_vec(&map) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    tracing::warn!("failed to persist dispatched state: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to serialize dispatched state: {e}"),
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

/// Parsed result from `gh issue list` output.
struct ParsedGhOutput {
    /// New issues not yet dispatched.
    new_issues: Vec<IncomingIssue>,
    /// All open issue numbers from the API response (for eviction).
    open_issue_ids: std::collections::HashSet<String>,
}

/// Parse the JSON output of `gh issue list --json number,title,body,url,labels,createdAt`
/// into new issues (filtering out dispatched) and the full set of open issue IDs.
fn parse_gh_output(
    json: &[u8],
    repo: &str,
    dispatched: &DashMap<String, TaskId>,
    project_root: Option<&std::path::Path>,
) -> anyhow::Result<ParsedGhOutput> {
    let issues: Vec<GhIssue> = serde_json::from_slice(json)?;
    let open_issue_ids: std::collections::HashSet<String> =
        issues.iter().map(|i| i.number.to_string()).collect();
    let new_issues = issues
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
            project_root: project_root.map(|p| p.to_path_buf()),
        })
        .collect();
    Ok(ParsedGhOutput {
        new_issues,
        open_issue_ids,
    })
}

#[async_trait]
impl IntakeSource for GitHubIssuesPoller {
    fn name(&self) -> &str {
        "github"
    }

    async fn poll(&self) -> anyhow::Result<Vec<IncomingIssue>> {
        let mut args = vec![
            "issue",
            "list",
            "--repo",
            &self.repo,
            "--state",
            "open",
            "--json",
            "number,title,body,url,labels,createdAt",
            "--limit",
            "1000",
        ];
        if !self.label.is_empty() {
            args.push("--label");
            args.push(&self.label);
        }
        let output = tokio::process::Command::new("gh")
            .args(&args)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh issue list failed: {stderr}");
        }

        let parsed = parse_gh_output(
            &output.stdout,
            &self.repo,
            &self.dispatched,
            self.project_root.as_deref(),
        )?;

        // Evict dispatched entries for issues no longer open (closed/deleted).
        // This prevents unbounded growth of the dispatched map.
        let stale: Vec<String> = self
            .dispatched
            .iter()
            .map(|e| e.key().clone())
            .filter(|id| !parsed.open_issue_ids.contains(id))
            .collect();
        if !stale.is_empty() {
            for id in &stale {
                self.dispatched.remove(id);
            }
            tracing::debug!(
                count = stale.len(),
                "intake: evicted dispatched entries for closed issues"
            );
            self.persist_dispatched();
        }

        Ok(parsed.new_issues)
    }

    async fn mark_dispatched(&self, external_id: &str, task_id: &TaskId) -> anyhow::Result<()> {
        self.dispatched
            .insert(external_id.to_string(), task_id.clone());
        self.persist_dispatched();
        Ok(())
    }

    async fn unmark_dispatched(&self, external_id: &str) {
        self.dispatched.remove(external_id);
        self.persist_dispatched();
    }

    async fn on_task_complete(
        &self,
        external_id: &str,
        result: &TaskCompletionResult,
    ) -> anyhow::Result<()> {
        // Failures that require manual intervention must stay in dispatched so the poller
        // does not immediately re-discover the open issue and hot-loop. The operator must
        // resolve the conflict before the issue can be re-dispatched.
        let needs_manual = result
            .error
            .as_deref()
            .map(|e| e.contains("manual resolution required"))
            .unwrap_or(false);
        // Remove transient failed or cancelled issues from dispatched so the poller can
        // retry them later if they remain open. Done tasks and permanent failures stay
        // dispatched to avoid re-processing.
        if (result.status.is_failure() || result.status.is_cancelled()) && !needs_manual {
            self.dispatched.remove(external_id);
            self.persist_dispatched();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskStatus;

    fn make_dispatched(ids: &[&str]) -> DashMap<String, TaskId> {
        let map = DashMap::new();
        for id in ids {
            map.insert(
                id.to_string(),
                harness_core::types::TaskId(format!("task-{id}")),
            );
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
        let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

        assert_eq!(parsed.new_issues.len(), 1);
        assert_eq!(parsed.open_issue_ids.len(), 1);
        assert!(parsed.open_issue_ids.contains("42"));
        let issue = &parsed.new_issues[0];
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
        let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

        assert_eq!(parsed.new_issues.len(), 1);
        assert_eq!(parsed.new_issues[0].external_id, "3");
        assert_eq!(parsed.open_issue_ids.len(), 3);
    }

    #[test]
    fn parse_gh_output_empty_array() {
        let json = b"[]";
        let dispatched = DashMap::new();
        let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();
        assert!(parsed.new_issues.is_empty());
        assert!(parsed.open_issue_ids.is_empty());
    }

    #[test]
    fn parse_gh_output_invalid_json_returns_error() {
        let json = b"not valid json";
        let dispatched = DashMap::new();
        let result = parse_gh_output(json, "owner/repo", &dispatched, None);
        assert!(result.is_err());
    }

    #[test]
    fn parse_gh_output_null_body_becomes_none_description() {
        let json = br#"[
            {"number": 5, "title": "No body", "body": null, "url": "u", "labels": [], "createdAt": null}
        ]"#;
        let dispatched = DashMap::new();
        let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();
        assert_eq!(parsed.new_issues[0].description, None);
    }

    #[test]
    fn parse_gh_output_returns_open_issue_ids_for_eviction() {
        let json = br#"[
            {"number": 10, "title": "A", "body": null, "url": "u1", "labels": [], "createdAt": null},
            {"number": 20, "title": "B", "body": null, "url": "u2", "labels": [], "createdAt": null}
        ]"#;

        // Issue 5 was dispatched but is no longer in the open list (closed).
        let dispatched = make_dispatched(&["5", "10"]);
        let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

        // Only issue 20 is new (10 already dispatched).
        assert_eq!(parsed.new_issues.len(), 1);
        assert_eq!(parsed.new_issues[0].external_id, "20");
        // open_issue_ids contains both open issues from the API.
        assert!(parsed.open_issue_ids.contains("10"));
        assert!(parsed.open_issue_ids.contains("20"));
        // Issue 5 is NOT in open_issue_ids — caller can evict it.
        assert!(!parsed.open_issue_ids.contains("5"));
    }

    #[test]
    fn on_task_complete_removes_cancelled_issue_from_dispatched() {
        let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: None,
        };
        let poller = GitHubIssuesPoller::new(&repo_cfg, None);
        let external_id = "42";
        poller.dispatched.insert(
            external_id.to_string(),
            harness_core::types::TaskId("task-42".to_string()),
        );

        let result = TaskCompletionResult {
            status: TaskStatus::Cancelled,
            pr_url: None,
            error: Some("cancelled".to_string()),
            summary: "cancelled".to_string(),
        };

        futures::executor::block_on(poller.on_task_complete(external_id, &result)).unwrap();

        assert!(!poller.dispatched.contains_key(external_id));
    }

    #[test]
    fn on_task_complete_manual_conflict_keeps_dispatched() {
        // Gate B: failures requiring manual resolution must NOT unmark the issue so
        // the poller cannot immediately re-discover the same conflict and hot-loop.
        let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: None,
        };
        let poller = GitHubIssuesPoller::new(&repo_cfg, None);
        let external_id = "77";
        poller.dispatched.insert(
            external_id.to_string(),
            harness_core::types::TaskId("task-77".to_string()),
        );

        let result = TaskCompletionResult {
            status: TaskStatus::Failed,
            pr_url: None,
            error: Some(
                "pr:77 is conflicting and rebase was not pushed; manual resolution required"
                    .to_string(),
            ),
            summary: "conflict gate fired".to_string(),
        };

        futures::executor::block_on(poller.on_task_complete(external_id, &result)).unwrap();

        assert!(
            poller.dispatched.contains_key(external_id),
            "issue must remain in dispatched after manual-resolution failure to prevent hot-loop"
        );
    }

    #[test]
    fn on_task_complete_transient_failure_removes_from_dispatched() {
        // Transient failures (e.g. rate limit, empty output) should unmark for retry.
        let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: None,
        };
        let poller = GitHubIssuesPoller::new(&repo_cfg, None);
        let external_id = "88";
        poller.dispatched.insert(
            external_id.to_string(),
            harness_core::types::TaskId("task-88".to_string()),
        );

        let result = TaskCompletionResult {
            status: TaskStatus::Failed,
            pr_url: None,
            error: Some("no PR number found in agent output; task requires PR_URL".to_string()),
            summary: "transient failure".to_string(),
        };

        futures::executor::block_on(poller.on_task_complete(external_id, &result)).unwrap();

        assert!(
            !poller.dispatched.contains_key(external_id),
            "issue must be removed from dispatched after transient failure so poller can retry"
        );
    }

    #[test]
    fn github_issues_poller_name_is_github() {
        let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: None,
        };
        let poller = GitHubIssuesPoller::new(&repo_cfg, None);
        assert_eq!(poller.name(), "github");
    }
}
