use crate::http::state::AppState;
use crate::task_runner::{TaskPhase, TaskState, TaskStatus};
use crate::workspace::WorkspaceEntry;
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorktreeResponse {
    pub task_id: String,
    pub branch: String,
    pub workspace_path: String,
    pub path_short: String,
    pub source_repo: String,
    pub repo: Option<String>,
    pub runtime_workflow_id: Option<String>,
    pub status: String,
    pub phase: String,
    pub description: Option<String>,
    pub turn: u32,
    pub max_turns: Option<u32>,
    pub created_at: String,
    pub duration_secs: u64,
    pub pr_url: Option<String>,
    pub project: Option<String>,
}

pub async fn worktrees(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    match list_worktrees(&state).await {
        Ok(worktrees) => (StatusCode::OK, Json(json!(worktrees))),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}

async fn list_worktrees(state: &AppState) -> anyhow::Result<Vec<WorktreeResponse>> {
    let Some(manager) = state.concurrency.workspace_mgr.as_ref() else {
        return Ok(Vec::new());
    };

    let default_max_turns = state.core.server.config.concurrency.max_turns;
    let now = SystemTime::now();
    let mut responses = Vec::new();

    for entry in manager.entries() {
        let task = state.core.tasks.get(&entry.task_id);
        if let Some(response) = response_from_entry(entry, task.as_ref(), default_max_turns, now) {
            responses.push(response);
        }
    }

    Ok(responses)
}

fn response_from_entry(
    entry: WorkspaceEntry,
    task: Option<&TaskState>,
    default_max_turns: Option<u32>,
    now: SystemTime,
) -> Option<WorktreeResponse> {
    match task {
        Some(task) => response_from_task(entry, task, default_max_turns, now),
        None => Some(response_from_workspace_entry(entry, default_max_turns, now)),
    }
}

fn response_from_task(
    entry: WorkspaceEntry,
    task: &TaskState,
    default_max_turns: Option<u32>,
    now: SystemTime,
) -> Option<WorktreeResponse> {
    if task.status.is_terminal() {
        return None;
    }

    let created_at: DateTime<Utc> = entry.created_at.into();
    let duration_secs = now
        .duration_since(entry.created_at)
        .unwrap_or_default()
        .as_secs();
    let max_turns = task
        .request_settings
        .as_ref()
        .and_then(|settings| settings.max_turns)
        .or(default_max_turns);

    Some(WorktreeResponse {
        task_id: entry.task_id.0,
        branch: entry.branch,
        workspace_path: entry.workspace_path.to_string_lossy().into_owned(),
        path_short: path_short(&entry.workspace_path),
        source_repo: entry.source_repo.to_string_lossy().into_owned(),
        repo: task.repo.clone(),
        runtime_workflow_id: entry.runtime_workflow_id,
        status: task.status.as_ref().to_string(),
        phase: phase_name(&task.phase).to_string(),
        description: task.description.clone(),
        turn: task.turn,
        max_turns,
        created_at: created_at.to_rfc3339(),
        duration_secs,
        pr_url: task.pr_url.clone(),
        project: task
            .project_root
            .as_ref()
            .map(|project| project.to_string_lossy().into_owned()),
    })
}

fn response_from_workspace_entry(
    entry: WorkspaceEntry,
    default_max_turns: Option<u32>,
    now: SystemTime,
) -> WorktreeResponse {
    let created_at: DateTime<Utc> = entry.created_at.into();
    let duration_secs = now
        .duration_since(entry.created_at)
        .unwrap_or_default()
        .as_secs();
    let source_repo = entry.source_repo.to_string_lossy().into_owned();

    WorktreeResponse {
        task_id: entry.task_id.0,
        branch: entry.branch,
        workspace_path: entry.workspace_path.to_string_lossy().into_owned(),
        path_short: path_short(&entry.workspace_path),
        source_repo: source_repo.clone(),
        repo: entry.repo,
        runtime_workflow_id: entry.runtime_workflow_id,
        status: TaskStatus::Implementing.as_ref().to_string(),
        phase: phase_name(&TaskPhase::Implement).to_string(),
        description: None,
        turn: 0,
        max_turns: default_max_turns,
        created_at: created_at.to_rfc3339(),
        duration_secs,
        pr_url: None,
        project: Some(source_repo),
    }
}

fn path_short(path: &Path) -> String {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return path.to_string_lossy().into_owned();
    }
    let start = components.len().saturating_sub(2);
    components[start..].join("/")
}

fn phase_name(phase: &TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Triage => "triage",
        TaskPhase::Plan => "plan",
        TaskPhase::Implement => "implement",
        TaskPhase::Review => "review",
        TaskPhase::Terminal => "terminal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::{PersistedRequestSettings, TaskStatus};
    use harness_core::config::misc::WorkspaceConfig;
    use harness_core::types::TaskId as CoreTaskId;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    fn entry(task_id: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            task_id: CoreTaskId(task_id.to_string()),
            workspace_path: PathBuf::from("/var/harness/workspaces/task-1"),
            source_repo: PathBuf::from("/Users/example/src/repo"),
            repo: Some("owner/repo".to_string()),
            runtime_workflow_id: Some("workflow-1".to_string()),
            branch: format!("harness/{task_id}"),
            created_at: UNIX_EPOCH + Duration::from_secs(100),
        }
    }

    #[test]
    fn path_short_uses_last_two_components() {
        assert_eq!(
            path_short(Path::new("/var/harness/workspaces/task-1")),
            "workspaces/task-1"
        );
    }

    #[test]
    fn response_from_task_uses_workspace_entry_and_task_fields() {
        let mut task = TaskState::new(CoreTaskId("task-1".to_string()));
        task.status = TaskStatus::Implementing;
        task.phase = TaskPhase::Review;
        task.turn = 3;
        task.repo = Some("owner/repo".to_string());
        task.description = Some("Fix worktree cards".to_string());
        task.pr_url = Some("https://github.com/owner/repo/pull/123".to_string());
        task.project_root = Some(PathBuf::from("/Users/example/src/repo"));
        task.request_settings = Some(PersistedRequestSettings {
            max_turns: Some(10),
            ..Default::default()
        });

        let response = response_from_task(
            entry("task-1"),
            &task,
            Some(20),
            UNIX_EPOCH + Duration::from_secs(850),
        )
        .expect("active task should render");

        assert_eq!(response.task_id, "task-1");
        assert_eq!(response.branch, "harness/task-1");
        assert_eq!(response.path_short, "workspaces/task-1");
        assert_eq!(response.status, "implementing");
        assert_eq!(response.phase, "review");
        assert_eq!(response.turn, 3);
        assert_eq!(response.max_turns, Some(10));
        assert_eq!(response.duration_secs, 750);
        assert_eq!(response.repo.as_deref(), Some("owner/repo"));
        assert_eq!(response.description.as_deref(), Some("Fix worktree cards"));
        assert_eq!(
            response.pr_url.as_deref(),
            Some("https://github.com/owner/repo/pull/123")
        );
        assert_eq!(response.project.as_deref(), Some("/Users/example/src/repo"));
    }

    #[test]
    fn response_from_task_skips_terminal_tasks() {
        let mut task = TaskState::new(CoreTaskId("task-1".to_string()));
        task.status = TaskStatus::Done;

        assert!(response_from_task(
            entry("task-1"),
            &task,
            Some(20),
            UNIX_EPOCH + Duration::from_secs(850),
        )
        .is_none());
    }

    #[test]
    fn response_from_task_falls_back_to_server_max_turns() {
        let mut task = TaskState::new(CoreTaskId("task-1".to_string()));
        task.status = TaskStatus::Implementing;

        let response = response_from_task(
            entry("task-1"),
            &task,
            Some(20),
            UNIX_EPOCH + Duration::from_secs(850),
        )
        .expect("active task should render");

        assert_eq!(response.max_turns, Some(20));
    }

    #[test]
    fn response_from_entry_includes_workspace_without_task_state() {
        let response = response_from_entry(
            entry("runtime-workspace-1"),
            None,
            Some(20),
            UNIX_EPOCH + Duration::from_secs(850),
        )
        .expect("active workspace should render without task state");

        assert_eq!(response.task_id, "runtime-workspace-1");
        assert_eq!(response.status, "implementing");
        assert_eq!(response.phase, "implement");
        assert_eq!(response.turn, 0);
        assert_eq!(response.max_turns, Some(20));
        assert_eq!(response.repo.as_deref(), Some("owner/repo"));
        assert_eq!(response.runtime_workflow_id.as_deref(), Some("workflow-1"));
        assert_eq!(response.project.as_deref(), Some("/Users/example/src/repo"));
        assert_eq!(response.description, None);
        assert_eq!(response.pr_url, None);
    }

    #[tokio::test]
    async fn api_route_returns_active_worktree_json() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-worktrees-api-")?;
        let mut state = crate::test_helpers::make_test_state(dir.path()).await?;
        let task_id = CoreTaskId("route-task-1".to_string());
        let workspace_path = dir.path().join("workspaces/route-task-1");
        let source_repo = dir.path().join("repo");
        let created_at = UNIX_EPOCH + Duration::from_secs(100);

        let manager = Arc::new(crate::workspace::WorkspaceManager::new(WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        })?);
        manager.active.insert(
            task_id.clone(),
            crate::workspace::ActiveWorkspace {
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.clone(),
                repo: Some("owner/repo".to_string()),
                runtime_workflow_id: None,
                branch: "harness/route-task-1".to_string(),
                created_at,
                owner_session: manager.owner_session.clone(),
                run_generation: 1,
            },
        );
        manager.active_paths.insert(workspace_path, task_id.clone());

        let mut task = TaskState::new(task_id);
        task.status = TaskStatus::Implementing;
        task.description = Some("Render active worktrees".to_string());
        task.repo = Some("owner/repo".to_string());
        task.turn = 2;
        task.request_settings = Some(PersistedRequestSettings {
            max_turns: Some(8),
            ..Default::default()
        });
        state.core.tasks.insert(&task).await;
        state.concurrency.workspace_mgr = Some(manager);

        let app = axum::Router::new()
            .route("/api/worktrees", axum::routing::get(worktrees))
            .with_state(Arc::new(state));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/api/worktrees")
                .body(axum::body::Body::empty())?,
        )
        .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let payload: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(payload[0]["task_id"], "route-task-1");
        assert_eq!(payload[0]["branch"], "harness/route-task-1");
        assert_eq!(payload[0]["path_short"], "workspaces/route-task-1");
        assert_eq!(payload[0]["description"], "Render active worktrees");
        assert_eq!(payload[0]["repo"], "owner/repo");
        assert_eq!(payload[0]["turn"], 2);
        assert_eq!(payload[0]["max_turns"], 8);
        Ok(())
    }

    #[tokio::test]
    async fn api_route_returns_active_worktree_without_task_state() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-worktrees-runtime-api-")?;
        let mut state = crate::test_helpers::make_test_state(dir.path()).await?;
        let task_id = CoreTaskId("runtime-workspace-1".to_string());
        let workspace_path = dir.path().join("workspaces/runtime-workspace-1");
        let source_repo = dir.path().join("repo");
        let created_at = UNIX_EPOCH + Duration::from_secs(100);

        let manager = Arc::new(crate::workspace::WorkspaceManager::new(WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        })?);
        manager.active.insert(
            task_id.clone(),
            crate::workspace::ActiveWorkspace {
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.clone(),
                repo: Some("owner/repo".to_string()),
                runtime_workflow_id: Some("workflow-1".to_string()),
                branch: "harness/runtime-workspace-1".to_string(),
                created_at,
                owner_session: manager.owner_session.clone(),
                run_generation: 1,
            },
        );
        manager.active_paths.insert(workspace_path, task_id);
        state.concurrency.workspace_mgr = Some(manager);

        let app = axum::Router::new()
            .route("/api/worktrees", axum::routing::get(worktrees))
            .with_state(Arc::new(state));
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/api/worktrees")
                .body(axum::body::Body::empty())?,
        )
        .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let payload: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(payload[0]["task_id"], "runtime-workspace-1");
        assert_eq!(payload[0]["status"], "implementing");
        assert_eq!(payload[0]["phase"], "implement");
        assert_eq!(payload[0]["repo"], "owner/repo");
        assert_eq!(payload[0]["runtime_workflow_id"], "workflow-1");
        assert_eq!(payload[0]["turn"], 0);
        assert_eq!(payload[0]["max_turns"], serde_json::Value::Null);
        assert_eq!(
            payload[0]["project"].as_str(),
            Some(source_repo.to_string_lossy().as_ref())
        );
        Ok(())
    }
}
