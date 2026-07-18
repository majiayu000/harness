use super::*;

#[test]
fn classify_pr_state_handles_merged_and_closed() {
    assert_eq!(
        classify_pr_state(&GitHubPullState {
            state: "closed".to_string(),
            merged_at: Some("2024-01-01T00:00:00Z".to_string()),
        }),
        GitHubState::PrMerged
    );
    assert_eq!(
        classify_pr_state(&GitHubPullState {
            state: "closed".to_string(),
            merged_at: None,
        }),
        GitHubState::PrClosed
    );
}

#[test]
fn classify_issue_state_preserves_completion_reason() {
    assert_eq!(
        classify_issue_state(&GitHubIssueState {
            state: "open".to_string(),
            state_reason: None,
        }),
        GitHubState::Open
    );
    assert_eq!(
        classify_issue_state(&GitHubIssueState {
            state: "closed".to_string(),
            state_reason: Some("not_planned".to_string()),
        }),
        GitHubState::IssueClosed
    );
    assert_eq!(
        classify_issue_state(&GitHubIssueState {
            state: "closed".to_string(),
            state_reason: Some("completed".to_string()),
        }),
        GitHubState::IssueCompleted
    );
}

#[test]
fn transition_mapping_matches_external_states() {
    assert_eq!(
        runtime_transition_for_github_state(GitHubState::PrMerged),
        Some(("done", "reconciled: PR merged externally"))
    );
    assert_eq!(
        runtime_transition_for_github_state(GitHubState::PrClosed),
        Some(("cancelled", "reconciled: PR closed externally"))
    );
    assert_eq!(
        runtime_transition_for_github_state(GitHubState::IssueCompleted),
        Some(("done", "reconciled: issue completed externally"))
    );
    assert_eq!(runtime_transition_for_github_state(GitHubState::Open), None);
}

#[test]
fn issue_terminal_state_rejects_non_terminal_targets() {
    use harness_workflow::issue_lifecycle::IssueLifecycleState;

    assert_eq!(
        reconciliation_apply::issue_terminal_state("done"),
        Some(IssueLifecycleState::Done)
    );
    assert_eq!(
        reconciliation_apply::issue_terminal_state("cancelled"),
        Some(IssueLifecycleState::Cancelled)
    );
    assert_eq!(reconciliation_apply::issue_terminal_state("blocked"), None);
}

#[test]
fn runtime_candidate_accepts_bound_pr_or_issue_only_target() {
    let active = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("workflow-1")
    .with_data(json!({
        "project_id": "/tmp/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let row_updated_at = active.updated_at + chrono::Duration::seconds(60);
    let candidate = runtime_candidate_from_instance(&active, row_updated_at).expect("candidate");
    assert_eq!(candidate.workflow_id, "workflow-1");
    assert_eq!(candidate.row_updated_at, row_updated_at);
    assert_eq!(candidate.pr_number, Some(77));
    assert_eq!(candidate.repo.as_deref(), Some("owner/repo"));

    let issue_only = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({ "repo": "owner/repo", "issue_number": 42 }));
    let candidate = runtime_candidate_from_instance(&issue_only, chrono::Utc::now())
        .expect("issue-only workflow should reconcile");
    assert_eq!(candidate.issue_number, Some(42));
    assert_eq!(candidate.pr_number, None);

    let terminal = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({ "pr_number": 77 }));
    assert!(runtime_candidate_from_instance(&terminal, chrono::Utc::now()).is_none());

    let missing_remote = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    );
    assert!(runtime_candidate_from_instance(&missing_remote, chrono::Utc::now()).is_none());
}

#[test]
fn ready_to_merge_open_alert_uses_row_age() {
    let now = chrono::Utc::now();
    let candidate = RuntimeWorkflowCandidate {
        workflow_id: "workflow-1".to_string(),
        state: "ready_to_merge".to_string(),
        row_updated_at: now,
        repo: Some("owner/repo".to_string()),
        project_root: None,
        issue_number: Some(42),
        pr_number: Some(77),
        pr_url: Some("https://github.com/owner/repo/pull/77".to_string()),
    };
    let settings = RuntimeWorkflowReconciliationSettings {
        ready_to_merge_min_age_secs: 0,
        ready_to_merge_alert_ttl_secs: 60,
    };
    assert!(ready_to_merge_open_alert(&candidate, GitHubState::Open, settings, now).is_none());
}
