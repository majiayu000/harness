use crate::http::AppState;
use crate::task_runner::{TaskPhase, TaskState};
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[derive(Debug, Serialize)]
pub struct WorktreeRecord {
    task_id: String,
    branch: String,
    workspace_path: String,
    path_short: String,
    source_repo: String,
    repo: String,
    status: String,
    phase: String,
    description: String,
    turn: u32,
    max_turns: Option<u32>,
    created_at: String,
    duration_secs: u64,
    pr_url: Option<String>,
    project: String,
}

pub async fn worktrees(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<Vec<WorktreeRecord>>) {
    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        return (StatusCode::OK, Json(Vec::new()));
    };

    let mut records = Vec::new();
    for entry in workspace_mgr.entries() {
        let Some(task) = state
            .core
            .tasks
            .get_with_db_fallback(&entry.task_id)
            .await
            .ok()
            .flatten()
        else {
            continue;
        };

        if task.status.is_terminal() {
            continue;
        }

        let max_turns = task
            .request_settings
            .as_ref()
            .and_then(|settings| settings.max_turns)
            .or(state.core.server.config.concurrency.max_turns);

        records.push(WorktreeRecord {
            task_id: entry.task_id.0.clone(),
            branch: entry.branch,
            workspace_path: entry.workspace_path.display().to_string(),
            path_short: shorten_path(&entry.workspace_path),
            source_repo: entry.source_repo.display().to_string(),
            repo: task
                .repo
                .clone()
                .unwrap_or_else(|| repo_fallback(&entry.source_repo)),
            status: task.status.as_ref().to_string(),
            phase: phase_name(&task.phase).to_string(),
            description: task
                .description
                .clone()
                .unwrap_or_else(|| "No description".to_string()),
            turn: task.turn,
            max_turns,
            created_at: format_system_time(entry.created_at),
            duration_secs: duration_secs(entry.created_at),
            pr_url: task.pr_url.clone(),
            project: project_name(&task, &entry.source_repo),
        });
    }

    (StatusCode::OK, Json(records))
}

fn shorten_path(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        path.display().to_string()
    }
}

fn repo_fallback(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn project_name(task: &TaskState, source_repo: &Path) -> String {
    task.project_root
        .as_deref()
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| repo_fallback(source_repo))
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

fn format_system_time(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn duration_secs(created_at: SystemTime) -> u64 {
    SystemTime::now()
        .duration_since(created_at)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
