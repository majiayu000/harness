use super::*;

async fn github_state_server_with_stalled_body(path: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled GitHub mock");
    let addr = listener.local_addr().expect("stalled GitHub mock address");
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 2048];
        let Ok(n) = socket.read(&mut buf).await else {
            return;
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        if !request.starts_with(&format!("GET {path} ")) {
            return;
        }
        let headers = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 128\r\nconnection: close\r\n\r\n{";
        if socket.write_all(headers.as_bytes()).await.is_ok() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    format!("http://{addr}")
}

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

#[tokio::test]
async fn exact_subject_fetch_rejects_identity_or_kind_mismatches() {
    let _env_guard = crate::workspace::test_support::async_env_lock()
        .lock()
        .await;

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/7",
        r#"{"number":7,"repository_url":"https://api.github.test/repos/owner/repo/","state":"open"}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    assert_eq!(
        fetch_exact_issue_state_with_token("owner/repo", 7, None).await,
        GitHubState::Open
    );
    drop(api_guard);

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/7",
        r#"{"number":7,"repository_url":"https://api.github.test/repos/owner/repo","state":"open","pull_request":{"url":"https://api.github.test/pulls/7"}}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    assert_eq!(
        fetch_exact_issue_state_with_token("owner/repo", 7, None).await,
        GitHubState::Unknown,
        "the GitHub issues endpoint also returns PRs, which are not exact issue matches"
    );
    drop(api_guard);

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/7",
        r#"{"number":7,"repository_url":"https://api.github.test/repos/owner/repo","state":"open","pull_request":null}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    assert_eq!(
        fetch_exact_issue_state_with_token("owner/repo", 7, None).await,
        GitHubState::Unknown,
        "the pull_request key identifies a PR even when its value is null"
    );
    drop(api_guard);

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/pulls/11",
        r#"{"number":12,"state":"open","merged_at":null,"base":{"repo":{"full_name":"owner/repo"}}}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    assert_eq!(
        fetch_exact_pr_state_with_token("owner/repo", 11, None).await,
        GitHubState::Unknown
    );
    drop(api_guard);

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/7",
        r#"{"number":7,"repository_url":"https://api.github.test/repos/other/repo","state":"open"}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    assert_eq!(
        fetch_exact_issue_state_with_token("owner/repo", 7, None).await,
        GitHubState::Unknown
    );
    drop(api_guard);
}

#[tokio::test]
async fn github_state_fetch_bounds_response_body_read() {
    let _env_guard = crate::workspace::test_support::async_env_lock()
        .lock()
        .await;
    let path = "/repos/owner/repo/issues/7";
    let api_base = github_state_server_with_stalled_body(path).await;
    let _api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build local GitHub mock client");

    let started = Instant::now();
    let result = github_get_json_with_client_timeout::<serde_json::Value>(
        &client,
        path,
        None,
        Duration::from_millis(50),
    )
    .await;

    assert!(result.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "body streaming must remain inside the request deadline"
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
    .with_server_data(json!({
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
    .with_server_data(json!({ "repo": "owner/repo", "issue_number": 42 }));
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
    .with_server_data(json!({ "pr_number": 77 }));
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
