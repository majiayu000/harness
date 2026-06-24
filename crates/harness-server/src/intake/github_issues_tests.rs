use super::*;
use crate::task_runner::TaskStatus;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;

struct FakeTaskChecker {
    existing: RwLock<HashSet<String>>,
}

#[async_trait]
impl DispatchedTaskChecker for FakeTaskChecker {
    async fn exists(&self, task_id: &TaskId) -> anyhow::Result<bool> {
        Ok(self.existing.read().await.contains(&task_id.0))
    }
}

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

fn repo_config() -> harness_core::config::intake::GitHubRepoConfig {
    harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    }
}

#[test]
fn parse_next_link_extracts_next_relation() {
    let link = r#"<https://api.github.com/repos/o/r/issues?page=2>; rel="next", <https://api.github.com/repos/o/r/issues?page=4>; rel="last""#;

    assert_eq!(
        parse_next_link(link),
        Some("https://api.github.com/repos/o/r/issues?page=2".to_string())
    );
}

#[test]
fn parse_next_link_accepts_whitespace_around_rel_equals() {
    let link = r#"<https://api.github.com/repos/o/r/issues?page=2>; rel = "next""#;

    assert_eq!(
        parse_next_link(link),
        Some("https://api.github.com/repos/o/r/issues?page=2".to_string())
    );
}

#[tokio::test]
async fn poll_follows_github_issue_pagination() -> anyhow::Result<()> {
    #[derive(Clone)]
    struct IssueServerState {
        base_url: String,
        requests: Arc<AtomicUsize>,
    }

    async fn issue_page(
        axum::extract::State(state): axum::extract::State<IssueServerState>,
        axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;

        state.requests.fetch_add(1, Ordering::SeqCst);
        let page = params.get("page").map(String::as_str).unwrap_or("1");
        let body = match page {
            "2" => {
                r#"[{"number":102,"title":"Second page","body":null,"url":"u102","labels":[{"name":"harness"}],"createdAt":null}]"#
            }
            _ => {
                r#"[{"number":101,"title":"First page","body":null,"url":"u101","labels":[{"name":"harness"}],"createdAt":null}]"#
            }
        };
        let mut response = (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
        if page == "1" {
            let next_url = format!(
                "{}/repos/owner/repo/issues?state=open&per_page=100&labels=harness&page=2",
                state.base_url
            );
            let header =
                match axum::http::HeaderValue::from_str(&format!("<{next_url}>; rel=\"next\"")) {
                    Ok(header) => header,
                    Err(error) => {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!("invalid test link header: {error}"),
                        )
                            .into_response();
                    }
                };
            response
                .headers_mut()
                .insert(axum::http::header::LINK, header);
        }
        response
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/repos/owner/repo/issues", axum::routing::get(issue_page))
        .with_state(IssueServerState {
            base_url: base_url.clone(),
            requests: Arc::clone(&requests),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let poller = GitHubIssuesPoller::new(&repo_config(), None);
    poller.dispatched.insert(
        "101".to_string(),
        harness_core::types::TaskId("task-101".to_string()),
    );
    poller.dispatched.insert(
        "999".to_string(),
        harness_core::types::TaskId("task-999".to_string()),
    );

    let issues = poller.poll_from_api_base_url(&base_url).await?;

    server.abort();
    match server.await {
        Err(join_error) if join_error.is_cancelled() => {}
        Err(join_error) => return Err(join_error.into()),
        Ok(Err(error)) => return Err(error.into()),
        Ok(Ok(())) => {}
    }
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].external_id, "102");
    assert!(poller.dispatched.contains_key("101"));
    assert!(!poller.dispatched.contains_key("999"));
    Ok(())
}

#[tokio::test]
async fn poll_keeps_dispatched_entries_when_pagination_repeats() -> anyhow::Result<()> {
    #[derive(Clone)]
    struct IssueServerState {
        base_url: String,
        requests: Arc<AtomicUsize>,
    }

    async fn repeating_issue_page(
        axum::extract::State(state): axum::extract::State<IssueServerState>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;

        state.requests.fetch_add(1, Ordering::SeqCst);
        let body = r#"[{"number":101,"title":"First page","body":null,"url":"u101","labels":[{"name":"harness"}],"createdAt":null}]"#;
        let mut response = (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
        response.headers_mut().insert(
            axum::http::header::LINK,
            match axum::http::HeaderValue::from_str(&format!(
                "<{}/repos/owner/repo/issues?state=open&per_page=100&labels=harness>; rel=\"next\"",
                state.base_url
            )) {
                Ok(header) => header,
                Err(error) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("invalid test link header: {error}"),
                    )
                        .into_response();
                }
            },
        );
        response
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/repos/owner/repo/issues",
            axum::routing::get(repeating_issue_page),
        )
        .with_state(IssueServerState {
            base_url: base_url.clone(),
            requests: Arc::clone(&requests),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let poller = GitHubIssuesPoller::new(&repo_config(), None);
    poller.dispatched.insert(
        "999".to_string(),
        harness_core::types::TaskId("task-999".to_string()),
    );

    let issues = poller.poll_from_api_base_url(&base_url).await?;

    server.abort();
    match server.await {
        Err(join_error) if join_error.is_cancelled() => {}
        Err(join_error) => return Err(join_error.into()),
        Ok(Err(error)) => return Err(error.into()),
        Ok(Ok(())) => {}
    }
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(issues.len(), 1);
    assert!(
        poller.dispatched.contains_key("999"),
        "incomplete pagination must not evict dispatched issues absent from the partial page"
    );
    Ok(())
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
fn parse_gh_output_accepts_api_url_and_html_url() {
    let json = br#"[
        {
            "number": 42,
            "title": "Fix login bug",
            "body": null,
            "url": "https://api.github.com/repos/owner/repo/issues/42",
            "html_url": "https://github.com/owner/repo/issues/42",
            "labels": []
        }
    ]"#;

    let dispatched = DashMap::new();
    let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

    assert_eq!(parsed.new_issues.len(), 1);
    assert_eq!(
        parsed.new_issues[0].url.as_deref(),
        Some("https://github.com/owner/repo/issues/42")
    );
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
fn parse_gh_output_filters_pull_requests_from_issue_endpoint() {
    let json = br#"[
        {
            "number": 10,
            "title": "Actual issue",
            "body": null,
            "url": "https://api.github.com/repos/owner/repo/issues/10",
            "html_url": "https://github.com/owner/repo/issues/10",
            "labels": [],
            "createdAt": null
        },
        {
            "number": 11,
            "title": "Open pull request",
            "body": null,
            "url": "https://api.github.com/repos/owner/repo/issues/11",
            "html_url": "https://github.com/owner/repo/pull/11",
            "pull_request": {
                "url": "https://api.github.com/repos/owner/repo/pulls/11",
                "html_url": "https://github.com/owner/repo/pull/11"
            },
            "labels": [],
            "createdAt": null
        }
    ]"#;

    let dispatched = DashMap::new();
    let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

    assert_eq!(parsed.new_issues.len(), 1);
    assert_eq!(parsed.new_issues[0].external_id, "10");
    assert!(parsed.open_issue_ids.contains("10"));
    assert!(!parsed.open_issue_ids.contains("11"));
}

#[test]
fn parse_gh_output_filters_canonical_dispatched_issue_keys() {
    let json = br#"[
        {"number": 1, "title": "A", "body": null, "url": "u1", "labels": [], "createdAt": null},
        {"number": 2, "title": "B", "body": null, "url": "u2", "labels": [], "createdAt": null},
        {"number": 3, "title": "C", "body": null, "url": "u3", "labels": [], "createdAt": null}
    ]"#;

    let dispatched = make_dispatched(&["1"]);
    dispatched.insert(
        "issue:2".to_string(),
        harness_core::types::TaskId("task-2".to_string()),
    );
    let parsed = parse_gh_output(json, "owner/repo", &dispatched, None).unwrap();

    assert_eq!(parsed.new_issues.len(), 1);
    assert_eq!(parsed.new_issues[0].external_id, "3");
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
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    let external_id = "42";
    poller.dispatched.insert(
        external_id.to_string(),
        harness_core::types::TaskId("task-42".to_string()),
    );

    let result = TaskCompletionResult {
        status: TaskStatus::Cancelled,
        failure_kind: None,
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
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    let external_id = "77";
    poller.dispatched.insert(
        external_id.to_string(),
        harness_core::types::TaskId("task-77".to_string()),
    );

    let result = TaskCompletionResult {
        status: TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::WorkspaceLifecycle),
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
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    let external_id = "88";
    poller.dispatched.insert(
        external_id.to_string(),
        harness_core::types::TaskId("task-88".to_string()),
    );

    let result = TaskCompletionResult {
        status: TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::WorkspaceLifecycle),
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
fn on_task_complete_transient_failure_removes_canonical_external_id_from_dispatched() {
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    poller.dispatched.insert(
        "88".to_string(),
        harness_core::types::TaskId("task-88".to_string()),
    );

    let result = TaskCompletionResult {
        status: TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::WorkspaceLifecycle),
        pr_url: None,
        error: Some("triage phase agent error".to_string()),
        summary: "transient failure".to_string(),
    };

    futures::executor::block_on(poller.on_task_complete("issue:88", &result)).unwrap();

    assert!(
        !poller.dispatched.contains_key("88"),
        "canonical external_id should remove the raw GitHub issue key"
    );
}

#[test]
fn on_task_complete_task_failure_keeps_dispatched() {
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    poller.dispatched.insert(
        "99".to_string(),
        harness_core::types::TaskId("task-99".to_string()),
    );

    let result = TaskCompletionResult {
        status: TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
        pr_url: None,
        error: Some("implementation test failure".to_string()),
        summary: "task failure".to_string(),
    };

    futures::executor::block_on(poller.on_task_complete("99", &result)).unwrap();

    assert!(
        poller.dispatched.contains_key("99"),
        "non-lifecycle task failures should stay dispatched"
    );
}

#[test]
fn github_issues_poller_accepts_configured_token() {
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller =
        GitHubIssuesPoller::new_with_token(&repo_cfg, None, Some(" configured ".to_string()));

    assert_eq!(
        crate::github_auth::resolve_github_token_from_sources(
            poller.github_token.as_deref(),
            Some("env-github"),
            Some("env-gh"),
        )
        .as_deref(),
        Some("configured")
    );
}

#[test]
fn runtime_constructor_without_data_dir_ignores_legacy_dispatched_file() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    std::fs::write(
        tmp.path().join("github_dispatched_owner_repo.json"),
        br#"{"42":"legacy-task"}"#,
    )?;
    let poller = GitHubIssuesPoller::new(&repo_config(), None);
    let parsed = parse_gh_output(
        br#"[{"number":42,"title":"Still open","body":null,"url":"u42","labels":[],"createdAt":null}]"#,
        "owner/repo",
        &poller.dispatched,
        None,
    )?;

    assert_eq!(parsed.new_issues.len(), 1);
    assert_eq!(parsed.new_issues[0].external_id, "42");
    Ok(())
}

#[tokio::test]
async fn reconcile_prunes_missing_dispatched_tasks_but_keeps_skip_markers() {
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let checker = Arc::new(FakeTaskChecker {
        existing: RwLock::new(
            ["live-task".to_string()]
                .into_iter()
                .collect::<HashSet<String>>(),
        ),
    });
    let poller = GitHubIssuesPoller::new(&repo_cfg, None).with_task_checker(checker);
    poller.dispatched.insert(
        "1".to_string(),
        harness_core::types::TaskId("missing-task".to_string()),
    );
    poller.dispatched.insert(
        "2".to_string(),
        harness_core::types::TaskId("live-task".to_string()),
    );
    poller.dispatched.insert(
        "3".to_string(),
        harness_core::types::TaskId("skip-3".to_string()),
    );

    let pruned = poller
        .reconcile_dispatched_with_store()
        .await
        .expect("reconcile should succeed");

    assert_eq!(pruned, 1, "only the missing real task should be pruned");
    assert!(!poller.dispatched.contains_key("1"));
    assert!(poller.dispatched.contains_key("2"));
    assert!(poller.dispatched.contains_key("3"));
}

#[tokio::test]
async fn reconcile_without_task_checker_is_noop() {
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, None);
    poller.dispatched.insert(
        "1".to_string(),
        harness_core::types::TaskId("missing-task".to_string()),
    );

    let pruned = poller
        .reconcile_dispatched_with_store()
        .await
        .expect("reconcile should succeed");

    assert_eq!(pruned, 0);
    assert!(poller.dispatched.contains_key("1"));
}

#[tokio::test]
async fn runtime_aware_checker_preserves_runtime_issue_handles() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _db_state_guard = crate::test_helpers::acquire_db_state_guard().await;
    let database_url = crate::test_helpers::test_database_url()?;
    let dir = tempfile::tempdir()?;
    let tasks = crate::task_runner::TaskStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "tasks"),
        Some(&database_url),
    )
    .await?;
    let runtime_store = harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
        Some(&database_url),
    )
    .await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-issue-handle");
    let labels = vec!["bug".to_string()];

    crate::workflow_runtime_submission::record_issue_submission(
        &runtime_store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 42,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
        },
    )
    .await?;

    let checker = RuntimeAwareDispatchedTaskChecker::new(tasks, Some(Arc::new(runtime_store)));

    assert!(checker.exists(&task_id).await?);
    assert!(
        !checker
            .exists(&TaskId::from_str("missing-runtime-handle"))
            .await?
    );
    Ok(())
}

// Surface 3 regression guard: the dispatched JSON file stores only
// {issue_id → task_id} pairs.  No workspace path must ever be written.
#[test]
fn surface3_dispatched_json_has_no_path_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_cfg = harness_core::config::intake::GitHubRepoConfig {
        repo: "owner/repo".to_string(),
        label: "harness".to_string(),
        project_root: None,
        auto_merge: None,
        merge_method: None,
        delete_branch: None,
        require_review_threads_resolved: None,
        require_clean_merge_state: None,
    };
    let poller = GitHubIssuesPoller::new(&repo_cfg, Some(tmp.path()));
    poller.dispatched.insert(
        "42".to_string(),
        harness_core::types::TaskId("task-abc123".to_string()),
    );
    poller.dispatched.insert(
        "99".to_string(),
        harness_core::types::TaskId("task-xyz456".to_string()),
    );
    poller.persist_dispatched();

    let persist_path = tmp.path().join("github_dispatched_owner_repo.json");
    let json =
        std::fs::read_to_string(&persist_path).expect("dispatched file should have been written");
    assert!(
        !json.contains("/workspaces/"),
        "dispatched JSON must not contain workspace paths, got: {json}"
    );
    let map: HashMap<String, String> =
        serde_json::from_str(&json).expect("dispatched JSON must parse");
    assert_eq!(map.len(), 2, "both entries should be persisted");
    assert!(
        map.values()
            .all(|v| !v.contains('/') || v.starts_with("skip-")),
        "task IDs must not be filesystem paths"
    );
}
