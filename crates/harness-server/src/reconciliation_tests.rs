use super::*;
use crate::task_runner::TaskState;
use harness_core::types::TaskId;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

fn make_task(
    id: &str,
    status: TaskStatus,
    pr_url: Option<&str>,
    external_id: Option<&str>,
) -> TaskState {
    let tid = TaskId(id.to_string());
    let mut task = TaskState::new(tid);
    task.status = status;
    task.pr_url = pr_url.map(|s| s.to_string());
    task.external_id = external_id.map(|s| s.to_string());
    task
}

fn async_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct ScopedEnvVar {
    key: String,
    original: Option<String>,
}

impl ScopedEnvVar {
    fn set(key: &str, value: &str) -> Self {
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            original,
        }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(value) = &self.original {
            unsafe { std::env::set_var(&self.key, value) };
        } else {
            unsafe { std::env::remove_var(&self.key) };
        }
    }
}

async fn github_state_server(routes: Vec<(&'static str, &'static str)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GitHub mock");
    let addr = listener.local_addr().expect("GitHub mock address");
    let routes: HashMap<String, &'static str> = routes
        .into_iter()
        .map(|(path, body)| (path.to_string(), body))
        .collect();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = [0_u8; 2048];
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or_default();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let (status, response_body) = match routes.get(&path).copied() {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", "{}"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}")
}

fn write_git_remote_config(path: &std::path::Path, origin: &str) {
    let dotgit = path.join(".git");
    std::fs::create_dir_all(&dotgit).expect("create .git");
    std::fs::write(
        dotgit.join("config"),
        format!("[remote \"origin\"]\n\turl = {origin}\n"),
    )
    .expect("write git config");
}

struct RuntimeStores {
    dir: tempfile::TempDir,
    task_store: Arc<TaskStore>,
    runtime_store: WorkflowRuntimeStore,
    issue_store: IssueWorkflowStore,
}

async fn open_runtime_stores() -> anyhow::Result<Option<RuntimeStores>> {
    let database_url = crate::test_helpers::test_database_url()?;
    let dir = tempfile::tempdir()?;
    let task_store = match TaskStore::open(&dir.path().join("tasks.db")).await {
        Ok(store) => store,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let runtime_store = match WorkflowRuntimeStore::open_with_database_url(
        &dir.path().join("runtime"),
        Some(&database_url),
    )
    .await
    {
        Ok(store) => store,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let issue_store = match IssueWorkflowStore::open_with_database_url(
        &dir.path().join("issue"),
        Some(&database_url),
    )
    .await
    {
        Ok(store) => store,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(Some(RuntimeStores {
        dir,
        task_store,
        runtime_store,
        issue_store,
    }))
}

fn ready_to_merge_config(min_age_secs: u64, alert_ttl_secs: u64) -> ReconciliationConfig {
    ReconciliationConfig {
        enabled: true,
        interval_secs: 300,
        max_gh_calls_per_minute: 20,
        ready_to_merge_min_age_secs: min_age_secs,
        ready_to_merge_alert_ttl_secs: alert_ttl_secs,
    }
}

async fn record_issue_ready_to_merge(
    issue_store: &IssueWorkflowStore,
    project_id: &str,
    issue_number: u64,
    task_id: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    issue_store
        .record_issue_scheduled(
            project_id,
            Some("owner/repo"),
            issue_number,
            task_id,
            &[],
            false,
        )
        .await?;
    issue_store
        .record_pr_detected(
            project_id,
            Some("owner/repo"),
            issue_number,
            task_id,
            pr_number,
            &format!("https://github.com/owner/repo/pull/{pr_number}"),
        )
        .await?;
    issue_store
        .record_ready_to_merge_with_fallback(
            project_id,
            Some("owner/repo"),
            pr_number,
            Some("ready to merge before reconciliation"),
            harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at: chrono::Utc::now(),
            },
        )
        .await?;
    Ok(())
}

fn ready_to_merge_instance(
    workflow_id: &str,
    project_id: &str,
    issue_number: u64,
    task_id: &str,
    pr_number: u64,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> WorkflowInstance {
    let mut instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(json!({
        "project_id": project_id,
        "repo": "owner/repo",
        "issue_number": issue_number,
        "task_id": task_id,
        "pr_number": pr_number,
        "pr_url": format!("https://github.com/owner/repo/pull/{pr_number}"),
    }));
    instance.updated_at = updated_at;
    instance
}

async fn persist_ready_to_merge_runtime(
    stores: &RuntimeStores,
    project_name: &str,
    issue_number: u64,
    task_id: &str,
    pr_number: u64,
    age_secs: i64,
    record_issue_workflow: bool,
) -> anyhow::Result<(String, String)> {
    let project_root = stores.dir.path().join(project_name);
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().to_string();
    if record_issue_workflow {
        record_issue_ready_to_merge(
            &stores.issue_store,
            &project_id,
            issue_number,
            task_id,
            pr_number,
        )
        .await?;
    }
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_id,
        Some("owner/repo"),
        issue_number,
    );
    let instance = ready_to_merge_instance(
        &workflow_id,
        &project_id,
        issue_number,
        task_id,
        pr_number,
        chrono::Utc::now() - chrono::Duration::seconds(age_secs),
    );
    stores.runtime_store.upsert_instance(&instance).await?;
    Ok((project_id, workflow_id))
}

#[test]
fn parse_external_id_handles_issue_pr_and_none() {
    assert_eq!(parse_external_id(Some("issue:42")), (Some(42), None));
    assert_eq!(parse_external_id(Some("pr:7")), (None, Some(7)));
    assert_eq!(parse_external_id(None), (None, None));
}

#[test]
fn candidate_from_task_skips_terminal() {
    let mut t = make_task(
        "x",
        TaskStatus::Done,
        Some("https://github.com/a/b/pull/1"),
        None,
    );
    assert!(candidate_from_task(&t).is_none());
    t.status = TaskStatus::Cancelled;
    assert!(candidate_from_task(&t).is_none());
}

#[test]
fn candidate_from_task_handles_references() {
    let t = make_task("x", TaskStatus::Implementing, None, None);
    assert!(candidate_from_task(&t).is_none());

    let t = make_task(
        "x",
        TaskStatus::Implementing,
        Some("https://github.com/a/b/pull/2"),
        None,
    );
    let c = candidate_from_task(&t).unwrap();
    assert!(c.pr_url.is_some());
    assert_eq!(c.issue_num, None);

    let mut t = make_task("x", TaskStatus::Pending, None, Some("issue:9"));
    t.project_root = Some(PathBuf::from("/tmp/projects/alpha"));
    let c = candidate_from_task(&t).unwrap();
    assert_eq!(c.project_root, Some(PathBuf::from("/tmp/projects/alpha")));

    let t = make_task("x", TaskStatus::Pending, None, Some("issue:9"));
    let c = candidate_from_task(&t).unwrap();
    assert_eq!(c.issue_num, Some(9));
    assert!(c.pr_url.is_none());
}

#[tokio::test]
async fn resolve_repo_slug_uses_candidate_project_root_when_repo_missing() {
    let repo_a = tempfile::tempdir().expect("repo a tempdir");
    let repo_b = tempfile::tempdir().expect("repo b tempdir");
    write_git_remote_config(repo_a.path(), "https://github.com/example/repo-a.git");
    write_git_remote_config(repo_b.path(), "https://github.com/example/repo-b.git");
    assert_eq!(
        crate::task_executor::pr_detection::detect_repo_slug(repo_a.path()).await,
        Some("example/repo-a".to_string())
    );

    let candidate = Candidate {
        id: TaskId("task-1".to_string()),
        pr_url: None,
        repo: None,
        project_root: Some(repo_b.path().to_path_buf()),
        issue_num: Some(9),
        pr_num_from_ext: None,
    };

    let mut cache = HashMap::new();
    let repo_slug = reconciliation_legacy::resolve_repo_slug(&candidate, &mut cache).await;
    assert_eq!(repo_slug, Some("example/repo-b".to_string()));
}

#[tokio::test]
async fn run_once_uses_each_task_project_root_when_repo_is_missing() {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return;
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let repo_a = tempfile::tempdir().expect("repo a tempdir");
    let repo_b = tempfile::tempdir().expect("repo b tempdir");
    write_git_remote_config(repo_a.path(), "https://github.com/example/repo-a.git");
    write_git_remote_config(repo_b.path(), "https://github.com/example/repo-b.git");

    let api_base = github_state_server(vec![
        ("/repos/example/repo-a/issues/9", r#"{"state":"open"}"#),
        ("/repos/example/repo-a/issues/41", r#"{"state":"open"}"#),
        ("/repos/example/repo-b/issues/9", r#"{"state":"closed"}"#),
    ])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let dir = tempfile::tempdir().expect("task store tempdir");
    let store = match TaskStore::open(&dir.path().join("tasks.db")).await {
        Ok(store) => store,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return,
        Err(err) => panic!("open task store: {err}"),
    };

    let mut repo_task = make_task("repo-task", TaskStatus::Pending, None, Some("issue:41"));
    repo_task.repo = Some("example/repo-a".to_string());
    repo_task.project_root = Some(repo_a.path().to_path_buf());
    store.insert(&repo_task).await;

    let mut repo_less_task =
        make_task("repo-less-task", TaskStatus::Pending, None, Some("issue:9"));
    repo_less_task.project_root = Some(repo_b.path().to_path_buf());
    store.insert(&repo_less_task).await;

    let report = run_once(&store, 20, false).await;

    assert_eq!(report.transitions.len(), 1);
    assert_eq!(report.transitions[0].task_id, repo_less_task.id.0);
    assert_eq!(
        report.transitions[0].reason,
        "reconciled: issue closed before PR"
    );

    let repo_task_after = store.get(&repo_task.id).expect("repo task remains");
    assert_eq!(repo_task_after.status, TaskStatus::Pending);

    let repo_less_after = store
        .get(&repo_less_task.id)
        .expect("repo-less task remains");
    assert_eq!(repo_less_after.status, TaskStatus::Cancelled);
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
fn classify_issue_state_handles_open_and_closed() {
    assert_eq!(
        classify_issue_state(&GitHubIssueState {
            state: "open".to_string(),
        }),
        GitHubState::Open
    );
    assert_eq!(
        classify_issue_state(&GitHubIssueState {
            state: "closed".to_string(),
        }),
        GitHubState::IssueClosed
    );
}

#[test]
fn transition_mapping_matches_external_states() {
    assert_eq!(
        transition_for_github_state(GitHubState::PrMerged),
        Some((TaskStatus::Done, "reconciled: PR merged externally"))
    );
    assert_eq!(
        transition_for_github_state(GitHubState::PrClosed),
        Some((TaskStatus::Cancelled, "reconciled: PR closed externally"))
    );
    assert_eq!(transition_for_github_state(GitHubState::Open), None);
}

#[test]
fn runtime_candidate_from_instance_requires_non_terminal_bound_pr() {
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
    assert_eq!(candidate.pr_number, 77);
    assert_eq!(candidate.repo.as_deref(), Some("owner/repo"));

    let terminal = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({ "pr_number": 77 }));
    assert!(runtime_candidate_from_instance(&terminal, chrono::Utc::now()).is_none());

    let missing_pr = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    );
    assert!(runtime_candidate_from_instance(&missing_pr, chrono::Utc::now()).is_none());
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
        pr_number: 77,
        pr_url: Some("https://github.com/owner/repo/pull/77".to_string()),
    };
    let settings = RuntimeWorkflowReconciliationSettings {
        ready_to_merge_min_age_secs: 0,
        ready_to_merge_alert_ttl_secs: 60,
    };
    assert!(ready_to_merge_open_alert(&candidate, GitHubState::Open, settings, now).is_none());
}

#[tokio::test]
async fn run_once_reconciles_runtime_merged_pr_workflow() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/77",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    let project_root = stores.dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy();
    stores
        .issue_store
        .record_issue_scheduled(&project_id, Some("owner/repo"), 42, "task-1", &[], false)
        .await?;
    stores
        .issue_store
        .record_pr_detected(
            &project_id,
            Some("owner/repo"),
            42,
            "task-1",
            77,
            "https://github.com/owner/repo/pull/77",
        )
        .await?;
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 42);
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id(&workflow_id)
    .with_data(json!({
        "project_id": project_id.as_ref(),
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "task-1",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    stores.runtime_store.upsert_instance(&instance).await?;

    let report = run_once_with_runtime_token(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        20,
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].from, "pr_open");
    assert_eq!(report.workflow_transitions[0].to, "done");
    assert!(report.workflow_transitions[0].applied);
    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(updated.state, "done");
    assert_eq!(updated.data["last_decision"], "reconcile_pr_merged");
    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 42)
        .await?
        .expect("issue workflow should exist");
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
    );
    Ok(())
}

#[tokio::test]
async fn run_once_reconciles_runtime_closed_pr_workflow() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/88",
        r#"{"state":"closed","merged_at":null}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    let project_root = stores.dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy();
    stores
        .issue_store
        .record_issue_scheduled(&project_id, Some("owner/repo"), 43, "task-2", &[], false)
        .await?;
    stores
        .issue_store
        .record_pr_detected(
            &project_id,
            Some("owner/repo"),
            43,
            "task-2",
            88,
            "https://github.com/owner/repo/pull/88",
        )
        .await?;
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 43);
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:43"),
    )
    .with_id(&workflow_id)
    .with_data(json!({
        "project_id": project_id.as_ref(),
        "repo": "owner/repo",
        "issue_number": 43,
        "task_id": "task-2",
        "pr_number": 88,
        "pr_url": "https://github.com/owner/repo/pull/88",
    }));
    stores.runtime_store.upsert_instance(&instance).await?;

    let report = run_once_with_runtime_token(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        20,
        false,
        None,
    )
    .await;

    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].to, "cancelled");
    assert!(report.workflow_transitions[0].applied);
    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(updated.state, "cancelled");
    assert_eq!(updated.data["last_decision"], "reconcile_pr_closed");
    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 43)
        .await?
        .expect("issue workflow should exist");
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn ready_to_merge_reconciliation_waits_for_configured_age() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/99",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    let (_, workflow_id) =
        persist_ready_to_merge_runtime(&stores, "project-young", 44, "task-3", 99, 120, false)
            .await?;
    let report = run_once_with_runtime_config(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &ready_to_merge_config(3600, 7200),
        false,
        None,
    )
    .await;
    assert!(report.workflow_transitions.is_empty());
    assert!(report.workflow_alerts.is_empty());
    let updated = stores
        .runtime_store
        .get_instance(&workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected ready_to_merge workflow to remain persisted"))?;
    assert_eq!(updated.state, "ready_to_merge");
    Ok(())
}

#[tokio::test]
async fn ready_to_merge_reconciliation_marks_merged_pr_done() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/100",
        r#"{"state":"closed","merged_at":"2026-05-10T00:00:00Z"}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    let (project_id, _) =
        persist_ready_to_merge_runtime(&stores, "project-merged", 45, "task-4", 100, 120, true)
            .await?;
    let report = run_once_with_runtime_config(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &ready_to_merge_config(0, 3600),
        false,
        None,
    )
    .await;
    assert_eq!(report.workflow_transitions.len(), 1);
    assert_eq!(report.workflow_transitions[0].from, "ready_to_merge");
    assert_eq!(report.workflow_transitions[0].to, "done");
    let issue_workflow = stores
        .issue_store
        .get_by_issue(&project_id, Some("owner/repo"), 45)
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected issue workflow to exist"))?;
    assert_eq!(
        issue_workflow.state,
        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
    );
    Ok(())
}

#[tokio::test]
async fn ready_to_merge_reconciliation_alerts_for_open_pr_after_ttl() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let api_base = github_state_server(vec![(
        "/repos/owner/repo/pulls/101",
        r#"{"state":"open","merged_at":null}"#,
    )])
    .await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let Some(stores) = open_runtime_stores().await? else {
        return Ok(());
    };
    persist_ready_to_merge_runtime(&stores, "project-open", 46, "task-5", 101, 120, false).await?;
    let report = run_once_with_runtime_config(
        &stores.task_store,
        Some(&stores.runtime_store),
        Some(&stores.issue_store),
        &ready_to_merge_config(0, 0),
        false,
        None,
    )
    .await;
    assert!(report.workflow_transitions.is_empty());
    assert_eq!(report.workflow_alerts.len(), 1);
    assert_eq!(report.workflow_alerts[0].pr_number, 101);
    assert_eq!(report.workflow_alerts[0].state, "ready_to_merge");
    Ok(())
}
