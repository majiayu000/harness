use axum::{
    body::{to_bytes, Body},
    http::Request,
    routing::get,
    Router,
};
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_server::{
    handlers::worktrees::worktrees,
    http::build_app_state,
    server::HarnessServer,
    task_db::TaskDb,
    task_runner::{PersistedRequestSettings, TaskPhase, TaskState, TaskStatus},
    thread_manager::ThreadManager,
};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tower::util::ServiceExt;

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-C", &repo.to_string_lossy()])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {:?} failed", args);
}

fn init_git_repo(repo: &Path) {
    run_git(repo, &["init", "-b", "main"]);
    run_git(repo, &["config", "user.email", "test@example.com"]);
    run_git(repo, &["config", "user.name", "Harness Test"]);
    std::fs::write(repo.join("README.md"), "hello\n").expect("write seed file");
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "init"]);
}

fn task_with_settings(
    id: &str,
    status: TaskStatus,
    turn: u32,
    phase: TaskPhase,
    project_root: &Path,
    max_turns: Option<u32>,
) -> TaskState {
    TaskState {
        id: harness_core::types::TaskId(id.to_string()),
        status,
        turn,
        pr_url: Some("https://github.com/majiayu000/harness/pull/999".to_string()),
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("882".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(project_root.to_path_buf()),
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: Some(format!("issue #{id}")),
        created_at: Some("2026-04-22T11:00:00Z".to_string()),
        priority: 0,
        phase,
        triage_output: None,
        plan_output: None,
        request_settings: max_turns.map(|value| PersistedRequestSettings {
            max_turns: Some(value),
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn get_worktrees_returns_live_workspace_records() -> anyhow::Result<()> {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping worktrees_api: DATABASE_URL is unset");
        return Ok(());
    };
    let Ok(pool) = harness_core::db::pg_open_pool(&database_url).await else {
        eprintln!("skipping worktrees_api: DATABASE_URL is not reachable");
        return Ok(());
    };
    pool.close().await;

    let dir = tempfile::tempdir()?;
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root)?;
    init_git_repo(&repo_root);

    let mut config = HarnessConfig::default();
    config.server.data_dir = dir.path().join("data");
    config.server.project_root = repo_root.clone();
    config.workspace.root = dir.path().join("worktrees");
    config.concurrency.max_turns = Some(21);
    config.server.database_url = Some(database_url);
    std::fs::create_dir_all(&config.server.data_dir)?;

    let db_path = harness_core::config::dirs::default_db_path(&config.server.data_dir, "tasks");
    let db = match TaskDb::open(&db_path).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("skipping worktrees_api: failed to open task db: {error}");
            return Ok(());
        }
    };
    db.insert(&task_with_settings(
        "active-override",
        TaskStatus::Implementing,
        3,
        TaskPhase::Implement,
        &repo_root,
        Some(7),
    ))
    .await?;
    db.insert(&task_with_settings(
        "active-fallback",
        TaskStatus::Reviewing,
        5,
        TaskPhase::Review,
        &repo_root,
        None,
    ))
    .await?;
    drop(db);

    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let state = match build_app_state(server).await {
        Ok(state) => Arc::new(state),
        Err(error) => {
            eprintln!("skipping worktrees_api: failed to build app state: {error}");
            return Ok(());
        }
    };
    let workspace_mgr = state
        .concurrency
        .workspace_mgr
        .as_ref()
        .expect("workspace manager enabled")
        .clone();

    workspace_mgr
        .create_workspace(
            &harness_core::types::TaskId("active-override".to_string()),
            &repo_root,
            "origin",
            "main",
        )
        .await?;
    workspace_mgr
        .create_workspace(
            &harness_core::types::TaskId("active-fallback".to_string()),
            &repo_root,
            "origin",
            "main",
        )
        .await?;

    let app = Router::new()
        .route("/api/worktrees", get(worktrees))
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/worktrees")
                .body(Body::empty())
                .expect("request"),
        )
        .await?;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
    let rows = body.as_array().expect("array response");
    assert_eq!(rows.len(), 2);

    let override_row = rows
        .iter()
        .find(|row| row["task_id"] == "active-override")
        .expect("override row");
    assert_eq!(override_row["branch"], "harness/active-override");
    assert_eq!(override_row["path_short"], "worktrees/active-override");
    assert_eq!(override_row["source_repo"], repo_root.display().to_string());
    assert_eq!(override_row["repo"], "majiayu000/harness");
    // Startup recovery resumes in-flight tasks as pending while preserving phase/checkpoints.
    assert_eq!(override_row["status"], "pending");
    assert_eq!(override_row["phase"], "implement");
    assert_eq!(override_row["turn"], 3);
    assert_eq!(override_row["max_turns"], 7);
    assert_eq!(
        override_row["project"],
        repo_root
            .file_name()
            .and_then(|name| name.to_str())
            .expect("repo dir name")
    );
    assert_eq!(
        override_row["pr_url"],
        "https://github.com/majiayu000/harness/pull/999"
    );
    assert!(override_row["created_at"]
        .as_str()
        .expect("created_at string")
        .ends_with('Z'));
    assert!(
        override_row["duration_secs"]
            .as_u64()
            .expect("duration secs")
            <= 60,
        "newly-created workspace should have a short lifetime in test"
    );

    let fallback_row = rows
        .iter()
        .find(|row| row["task_id"] == "active-fallback")
        .expect("fallback row");
    assert_eq!(fallback_row["status"], "pending");
    assert_eq!(fallback_row["phase"], "review");
    assert_eq!(fallback_row["max_turns"], 21);

    state.observability.events.shutdown().await;
    Ok(())
}
