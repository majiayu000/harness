use chrono::Utc;
use harness_workflow::issue_lifecycle::{workflow_id, IssueLifecycleState, IssueWorkflowStore};
use harness_workflow::runtime::{
    RemoteFactSnapshot, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType,
    WorkflowDecision, WorkflowDefinition, WorkflowEvidence, WorkflowInstance, WorkflowRuntimeStore,
    WorkflowSubject, WorkflowSubmissionDecisionTransition, GITHUB_ISSUE_PR_DEFINITION_ID,
    QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
};
use serde_json::json;
use std::path::Path;

use super::github_coverage_recovery::{
    cancel_superseded_commands, preserve_submission_handles, recovered_workflow_data,
};
use super::github_issue_links::{
    closing_pr_belongs_to_repo, fetch_github_issue_closing_prs_with_client,
    recovery_expected_base_ref,
};
use super::IncomingIssue;
use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot_with_client, github_graphql_url, pr_readiness_for_snapshot,
    GitHubPrSnapshotTarget, PrReadiness,
};
use crate::workflow_runtime_pr_feedback::pr_detection::ClosingPullRequestCandidate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitHubIssueCoverage {
    Covered { source: &'static str, state: String },
    Uncovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveredWorkflowPersistence {
    Persisted,
    ExistingCoverage(String),
    Rejected,
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
            | "quality_gate_pending"
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
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    github_token: Option<&str>,
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
            if runtime_issue_state_is_covered(&workflow.state)
                && !recovered_closed_pr_requires_lookup(&workflow)
            {
                return Ok(GitHubIssueCoverage::Covered {
                    source: "workflow_runtime",
                    state: workflow.state,
                });
            }
        }
    }

    let Some(runtime_store) = runtime_store else {
        return Ok(GitHubIssueCoverage::Uncovered);
    };
    recover_github_pr_coverage(
        runtime_store,
        project_root,
        project_id,
        repo,
        issue_number,
        github_token,
    )
    .await
}

fn recovered_closed_pr_requires_lookup(workflow: &WorkflowInstance) -> bool {
    workflow.state == "cancelled"
        && workflow
            .data
            .get("coverage_recovered_from_github")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && workflow.data.get("pr_number").is_some()
}

async fn recover_github_pr_coverage(
    runtime_store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubIssueCoverage> {
    let client = reqwest::Client::new();
    recover_github_pr_coverage_with_client(
        runtime_store,
        project_root,
        project_id,
        repo,
        issue_number,
        github_token,
        &client,
        &github_graphql_url(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn recover_github_pr_coverage_with_client(
    runtime_store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    github_token: Option<&str>,
    client: &reqwest::Client,
    graphql_url: &str,
) -> anyhow::Result<GitHubIssueCoverage> {
    let issue_links = fetch_github_issue_closing_prs_with_client(
        client,
        repo,
        issue_number,
        github_token,
        graphql_url,
    )
    .await?;
    let expected_base_ref =
        recovery_expected_base_ref(runtime_store, project_root, project_id, repo, issue_number)
            .await?;
    let mut snapshots = Vec::new();

    for candidate in issue_links.candidates {
        if !closing_pr_belongs_to_repo(&candidate, repo) {
            continue;
        }
        let target = GitHubPrSnapshotTarget::new(&candidate.repo_slug, candidate.number)?
            .with_expected_base_ref(&expected_base_ref);
        let artifacts =
            fetch_github_pr_snapshot_with_client(client, &target, github_token, graphql_url)
                .await?;
        let remote_fact = artifacts.remote_fact_snapshot()?;
        let readiness = pr_readiness_for_snapshot(&artifacts.normalized_snapshot);
        snapshots.push((
            candidate,
            artifacts.normalized_snapshot,
            remote_fact,
            readiness,
        ));
    }

    for (_, _, remote_fact, _) in &snapshots {
        runtime_store
            .upsert_remote_fact_snapshot(remote_fact)
            .await?;
    }

    let Some((candidate, snapshot, remote_fact, readiness)) = snapshots
        .into_iter()
        .filter(|(_, _, _, readiness)| *readiness != PrReadiness::ClosedUnmerged)
        .max_by_key(|(candidate, _, _, readiness)| {
            (*readiness == PrReadiness::Merged, candidate.number)
        })
    else {
        return Ok(GitHubIssueCoverage::Uncovered);
    };

    let state = recovered_runtime_state(readiness);
    runtime_store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await?;
    let persistence = persist_recovered_workflow(
        runtime_store,
        project_root,
        project_id,
        repo,
        issue_number,
        &candidate,
        &snapshot,
        &remote_fact.fact_hash,
        state,
    )
    .await?;
    match persistence {
        RecoveredWorkflowPersistence::Persisted => Ok(GitHubIssueCoverage::Covered {
            source: "github_closing_pr",
            state: state.to_string(),
        }),
        RecoveredWorkflowPersistence::ExistingCoverage(state) => Ok(GitHubIssueCoverage::Covered {
            source: "workflow_runtime",
            state,
        }),
        RecoveredWorkflowPersistence::Rejected => Ok(GitHubIssueCoverage::Uncovered),
    }
}

fn recovered_runtime_state(readiness: PrReadiness) -> &'static str {
    match readiness {
        PrReadiness::NeedsFeedbackRepair | PrReadiness::NeedsCiRepair => "awaiting_feedback",
        PrReadiness::WaitingForChecks | PrReadiness::WaitingForMergeability => "pr_open",
        PrReadiness::ReadyToMerge => "quality_gate_pending",
        PrReadiness::Merged => "done",
        PrReadiness::ClosedUnmerged => "cancelled",
    }
}

#[allow(clippy::too_many_arguments)]
async fn persist_recovered_workflow(
    runtime_store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    candidate: &ClosingPullRequestCandidate,
    snapshot: &serde_json::Value,
    fact_hash: &str,
    state: &str,
) -> anyhow::Result<RecoveredWorkflowPersistence> {
    let id = workflow_id(project_id, Some(repo), issue_number);
    let mut data = recovered_workflow_data(
        project_root,
        project_id,
        repo,
        issue_number,
        candidate,
        snapshot,
        fact_hash,
        state,
    );

    if state == "quality_gate_pending" {
        return persist_recovered_quality_gate(
            runtime_store,
            project_id,
            repo,
            issue_number,
            candidate,
            data,
        )
        .await;
    }

    if let Some(mut existing) = runtime_store.get_instance(&id).await? {
        if existing.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
            return Ok(RecoveredWorkflowPersistence::Rejected);
        }
        if runtime_issue_state_is_covered(&existing.state)
            && !recovered_closed_pr_requires_lookup(&existing)
        {
            return Ok(RecoveredWorkflowPersistence::ExistingCoverage(
                existing.state,
            ));
        }
        preserve_submission_handles(&mut data, &existing.data);
        cancel_superseded_commands(runtime_store, &id).await?;
        existing.state = state.to_string();
        existing.data = data;
        existing.version = existing.version.saturating_add(1);
        runtime_store.upsert_instance(&existing).await?;
        return Ok(RecoveredWorkflowPersistence::Persisted);
    }

    let recovered = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(id)
    .with_data(data);
    if runtime_store.insert_instance_if_absent(&recovered).await? {
        return Ok(RecoveredWorkflowPersistence::Persisted);
    }
    Ok(runtime_store
        .get_instance(&recovered.id)
        .await?
        .filter(|workflow| runtime_issue_state_is_covered(&workflow.state))
        .map(|workflow| RecoveredWorkflowPersistence::ExistingCoverage(workflow.state))
        .unwrap_or(RecoveredWorkflowPersistence::Rejected))
}

async fn persist_recovered_quality_gate(
    runtime_store: &WorkflowRuntimeStore,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    candidate: &ClosingPullRequestCandidate,
    mut data: serde_json::Value,
) -> anyhow::Result<RecoveredWorkflowPersistence> {
    let id = workflow_id(project_id, Some(repo), issue_number);
    let existing = runtime_store.get_instance(&id).await?;
    if let Some(workflow) = existing.as_ref() {
        if workflow.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
            return Ok(RecoveredWorkflowPersistence::Rejected);
        }
        if runtime_issue_state_is_covered(&workflow.state)
            && !recovered_closed_pr_requires_lookup(workflow)
        {
            return Ok(RecoveredWorkflowPersistence::ExistingCoverage(
                workflow.state.clone(),
            ));
        }
        preserve_submission_handles(&mut data, &workflow.data);
        cancel_superseded_commands(runtime_store, &id).await?;
    }

    let create_if_missing = existing.is_none();
    let initial = existing.unwrap_or_else(|| {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "awaiting_feedback",
            WorkflowSubject::new("issue", format!("issue:{issue_number}")),
        )
        .with_id(id.clone())
        .with_data(data.clone())
    });
    let task_id = format!("github-issue:{repo}:issue:{issue_number}");
    let command = WorkflowCommand::new(
        WorkflowCommandType::StartChildWorkflow,
        format!("quality-gate:{task_id}:{}:run", candidate.number),
        json!({
            "definition_id": QUALITY_GATE_DEFINITION_ID,
            "subject_key": format!("pr:{}", candidate.number),
            "child_activity": QUALITY_GATE_ACTIVITY,
            "pr_number": candidate.number,
            "pr_url": candidate.url,
            "validation_commands": [],
        }),
    );
    let decision = WorkflowDecision::new(
        &id,
        &initial.state,
        "recover_pr_quality_gate",
        "quality_gate_pending",
        "Recovered a ready closing PR; run the quality gate before merge eligibility.",
    )
    .with_command(command)
    .with_evidence(WorkflowEvidence::new(
        "server_pr_snapshot",
        "GitHub reported a green, approved, mergeable closing PR.",
    ))
    .high_confidence();
    let mut final_instance = initial.clone();
    final_instance.state = decision.next_state.clone();
    final_instance.data = data;
    final_instance.version = final_instance.version.saturating_add(1);
    let committed = runtime_store
        .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
            workflow_id: &id,
            expected_state: &initial.state,
            expected_version: initial.version,
            create_if_missing: create_if_missing.then_some(&initial),
            event_id: None,
            new_event_id: None,
            event_type: "ClosingPrCoverageRecovered",
            source: "github_intake_coverage",
            payload: json!({
                "issue_number": issue_number,
                "pr_number": candidate.number,
                "target_state": "quality_gate_pending",
            }),
            decision: &decision,
            existing_record: None,
            rejection_reason: None,
            final_instance: Some(&final_instance),
            command_status: WorkflowCommandStatus::Pending,
            prompt_payload: None,
        })
        .await?;
    if committed.is_some()
        || runtime_store
            .get_instance(&id)
            .await?
            .is_some_and(|workflow| workflow.state == "quality_gate_pending")
    {
        return Ok(RecoveredWorkflowPersistence::Persisted);
    }
    Ok(runtime_store
        .get_instance(&id)
        .await?
        .filter(|workflow| runtime_issue_state_is_covered(&workflow.state))
        .map(|workflow| RecoveredWorkflowPersistence::ExistingCoverage(workflow.state))
        .unwrap_or(RecoveredWorkflowPersistence::Rejected))
}

#[cfg(test)]
#[path = "github_coverage_recovery_tests.rs"]
mod recovery_tests;

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
        assert!(runtime_issue_state_is_covered("quality_gate_pending"));
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
                check_github_issue_coverage(
                    None,
                    Some(&store),
                    &project_root,
                    &project_id,
                    repo,
                    issue_number,
                    None,
                )
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

            let coverage = check_github_issue_coverage(
                None,
                Some(&store),
                &project_root,
                &project_id,
                repo,
                issue_number,
                None,
            )
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
