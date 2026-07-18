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
            | "replanning"
            | "pr_open"
            | "local_review_gate"
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
    use harness_workflow::runtime::{
        RuntimeKind, WorkflowCommand, WorkflowCommandStatus, WorkflowInstance,
        WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome,
        WorkflowRuntimeRecoveryRequest, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    };

    use super::*;
    use harness_core::types::TaskId;

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
    fn runtime_coverage_includes_stopped_and_recovered_states() {
        assert!(runtime_issue_state_is_covered("ready_to_merge"));
        assert!(runtime_issue_state_is_covered("blocked"));
        assert!(runtime_issue_state_is_covered("failed"));
        assert!(runtime_issue_state_is_covered("replanning"));
        assert!(runtime_issue_state_is_covered("local_review_gate"));
        assert!(!runtime_issue_state_is_covered("unknown_scanning"));
    }

    #[tokio::test]
    async fn github_coverage_gate_observes_recovered_runtime_state() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-github-coverage-")?;
        let store = WorkflowRuntimeStore::open_with_database_url(
            &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
            Some(&crate::test_helpers::test_database_url()?),
        )
        .await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy().into_owned();
        let repo = "owner/repo";

        for (issue_number, stopped_state, action) in [
            (156_700, "blocked", WorkflowRuntimeRecoveryAction::Unblock),
            (156_701, "failed", WorkflowRuntimeRecoveryAction::Retry),
        ] {
            let workflow = store_stopped_replan_workflow(
                &store,
                &project_id,
                repo,
                issue_number,
                stopped_state,
            )
            .await?;

            assert_eq!(
                check_github_issue_coverage(None, Some(&store), &project_id, repo, issue_number,)
                    .await?,
                GitHubIssueCoverage::Covered {
                    source: "workflow_runtime",
                    state: stopped_state.to_string(),
                }
            );

            let recovered = store
                .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
                    workflow_id: &workflow.id,
                    action,
                    reason: "external blocker resolved",
                    actor: "operator",
                    target_state: None,
                    evidence: &[],
                })
                .await?;
            assert!(matches!(
                recovered,
                WorkflowRuntimeRecoveryOutcome::Recovered { ref workflow, .. }
                    if workflow.state == "replanning"
            ));

            let coverage =
                check_github_issue_coverage(None, Some(&store), &project_id, repo, issue_number)
                    .await?;
            if coverage == GitHubIssueCoverage::Uncovered {
                let task_id =
                    TaskId::from_str(&format!("github-issue:{repo}:issue:{issue_number}"));
                crate::workflow_runtime_submission::record_issue_submission(
                    &store,
                    crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                        project_root: &project_root,
                        repo: Some(repo),
                        issue_number,
                        task_id: &task_id,
                        labels: &[],
                        force_execute: true,
                        additional_prompt: None,
                        depends_on: &[],
                        dependencies_blocked: false,
                        source: Some("github"),
                        external_id: Some("recovered-coverage-test"),
                        remote_fact_hash: None,
                        author_trust_class: None,
                    },
                )
                .await?;
            }

            let pending_command_count = store
                .commands_for(&workflow.id)
                .await?
                .into_iter()
                .filter(|command| command.status == WorkflowCommandStatus::Pending)
                .count();
            assert_eq!(
                pending_command_count, 1,
                "the intake tick must not enqueue work beside the recovery replay"
            );
            assert_eq!(
                coverage,
                GitHubIssueCoverage::Covered {
                    source: "workflow_runtime",
                    state: "replanning".to_string(),
                }
            );
        }

        Ok(())
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

    async fn store_stopped_replan_workflow(
        store: &WorkflowRuntimeStore,
        project_id: &str,
        repo: &str,
        issue_number: u64,
        stopped_state: &str,
    ) -> anyhow::Result<WorkflowInstance> {
        let workflow_id = workflow_id(project_id, Some(repo), issue_number);
        let mut workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            stopped_state,
            WorkflowSubject::new("issue", format!("issue:{issue_number}")),
        )
        .with_id(workflow_id)
        .with_data(json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
            "error_kind": "timeout",
            "last_stop": {
                "state": stopped_state,
                "activity": "replan_issue",
                "error_kind": "timeout",
            },
        }));
        store.upsert_instance(&workflow).await?;

        let command = WorkflowCommand::enqueue_activity(
            "replan_issue",
            format!("stopped-replan-{issue_number}"),
        );
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        let runtime_job = store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-test",
                command.command,
            )
            .await?;
        workflow.data["last_stop"]["runtime_job_id"] = json!(runtime_job.id);
        store.upsert_instance(&workflow).await?;
        Ok(workflow)
    }
}
