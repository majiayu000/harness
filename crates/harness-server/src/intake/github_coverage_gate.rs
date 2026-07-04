use chrono::Utc;
use harness_workflow::issue_lifecycle::{workflow_id, IssueLifecycleState, IssueWorkflowStore};
use harness_workflow::runtime::{RemoteFactSnapshot, WorkflowRuntimeStore};
use serde_json::json;

use super::IncomingIssue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitHubIssueCoverage {
    Covered { source: &'static str, state: String },
    Uncovered,
}

pub(crate) fn issue_lifecycle_state_is_covered(state: IssueLifecycleState) -> bool {
    matches!(
        state,
        IssueLifecycleState::Discovered
            | IssueLifecycleState::AwaitingDependencies
            | IssueLifecycleState::Scheduled
            | IssueLifecycleState::Implementing
            | IssueLifecycleState::PrOpen
            | IssueLifecycleState::AwaitingFeedback
            | IssueLifecycleState::FeedbackClaimed
            | IssueLifecycleState::AddressingFeedback
            | IssueLifecycleState::ReadyToMerge
            | IssueLifecycleState::Merging
            | IssueLifecycleState::Done
            | IssueLifecycleState::Blocked
            | IssueLifecycleState::Failed
            | IssueLifecycleState::Cancelled
    )
}

pub(crate) fn runtime_issue_state_is_covered(state: &str) -> bool {
    matches!(
        state,
        "discovered"
            | "awaiting_dependencies"
            | "scheduled"
            | "implementing"
            | "pr_open"
            | "awaiting_feedback"
            | "feedback_claimed"
            | "addressing_feedback"
            | "ready_to_merge"
            | "merging"
            | "done"
            | "blocked"
            | "failed"
            | "cancelled"
    )
}

pub(crate) fn issue_remote_fact_snapshot(
    repo: &str,
    issue_number: u64,
    issue: &IncomingIssue,
) -> anyhow::Result<RemoteFactSnapshot> {
    let subject_number = i64::try_from(issue_number)?;
    let facts = json!({
        "provider": "github",
        "repo": repo,
        "subject_type": "issue",
        "number": issue_number,
        "title": issue.title,
        "url": issue.url,
        "labels": issue.labels,
        "state": "open",
        "created_at": issue.created_at,
    });
    let mut snapshot = RemoteFactSnapshot::new(
        "github",
        repo,
        "issue",
        subject_number,
        "open",
        facts,
        Utc::now(),
    );
    if let Some(url) = issue.url.as_ref() {
        snapshot = snapshot.with_subject_url(url.clone());
    }
    Ok(snapshot)
}

pub(crate) async fn record_issue_remote_fact_snapshot(
    runtime_store: Option<&WorkflowRuntimeStore>,
    repo: &str,
    issue_number: u64,
    issue: &IncomingIssue,
) -> anyhow::Result<Option<String>> {
    let Some(runtime_store) = runtime_store else {
        return Ok(None);
    };
    let snapshot = issue_remote_fact_snapshot(repo, issue_number, issue)?;
    let fact_hash = snapshot.fact_hash.clone();
    runtime_store.upsert_remote_fact_snapshot(&snapshot).await?;
    Ok(Some(fact_hash))
}

pub(crate) async fn check_github_issue_coverage(
    issue_store: Option<&IssueWorkflowStore>,
    runtime_store: Option<&WorkflowRuntimeStore>,
    project_id: &str,
    repo: &str,
    issue_number: u64,
) -> anyhow::Result<GitHubIssueCoverage> {
    if let Some(issue_store) = issue_store {
        if let Some(workflow) = issue_store
            .get_by_issue(project_id, Some(repo), issue_number)
            .await?
        {
            if issue_lifecycle_state_is_covered(workflow.state) {
                return Ok(GitHubIssueCoverage::Covered {
                    source: "issue_workflow",
                    state: serde_json::to_value(workflow.state)?
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string(),
                });
            }
        }
    }

    if let Some(runtime_store) = runtime_store {
        let id = workflow_id(project_id, Some(repo), issue_number);
        if let Some(workflow) = runtime_store.get_instance(&id).await? {
            if runtime_issue_state_is_covered(&workflow.state) {
                return Ok(GitHubIssueCoverage::Covered {
                    source: "workflow_runtime",
                    state: workflow.state,
                });
            }
        }
    }

    Ok(GitHubIssueCoverage::Uncovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_coverage_includes_quiescent_and_terminal_states() {
        for state in [
            IssueLifecycleState::Discovered,
            IssueLifecycleState::AwaitingDependencies,
            IssueLifecycleState::ReadyToMerge,
            IssueLifecycleState::Merging,
            IssueLifecycleState::Done,
            IssueLifecycleState::Blocked,
            IssueLifecycleState::Failed,
            IssueLifecycleState::Cancelled,
        ] {
            assert!(issue_lifecycle_state_is_covered(state), "{state:?}");
        }
    }

    #[test]
    fn runtime_coverage_includes_ready_to_merge_and_blocked() {
        assert!(runtime_issue_state_is_covered("ready_to_merge"));
        assert!(runtime_issue_state_is_covered("blocked"));
        assert!(runtime_issue_state_is_covered("failed"));
        assert!(!runtime_issue_state_is_covered("unknown_scanning"));
    }

    #[test]
    fn issue_remote_fact_snapshot_hash_is_stable_for_same_issue() {
        let issue = IncomingIssue {
            source: "github".to_string(),
            external_id: "7".to_string(),
            identifier: "#7".to_string(),
            title: "Fix it".to_string(),
            description: Some("body".to_string()),
            repo: Some("owner/repo".to_string()),
            url: Some("https://github.com/owner/repo/issues/7".to_string()),
            priority: None,
            labels: vec!["harness".to_string()],
            created_at: None,
            author_trust_class: harness_core::config::isolation::IsolationTrustClass::Trusted,
            project_root: None,
        };

        let left = issue_remote_fact_snapshot("owner/repo", 7, &issue).expect("snapshot");
        let right = issue_remote_fact_snapshot("owner/repo", 7, &issue).expect("snapshot");

        assert_eq!(left.fact_hash, right.fact_hash);
    }
}
