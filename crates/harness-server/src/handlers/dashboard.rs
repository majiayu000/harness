use crate::{http::AppState, task_runner::TaskStatus};
use axum::{extract::State, http::StatusCode, Json};
use harness_core::EventFilters;
use harness_observe::QualityGrader;
use serde_json::{json, Value};
use std::sync::Arc;

/// Lazily initialized server start time. Set on the first dashboard request.
static SERVER_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// GET /api/dashboard — JSON summary of all registered projects and global concurrency.
///
/// Per-project entries include only live queue stats (running/queued) because
/// `TaskState` carries no `project_root` field, making per-project historical
/// counts (done/failed) and per-project PR/grade unavailable without a schema
/// change. Global metrics (done, failed, latest_pr, grade, uptime) are reported
/// in the top-level `global` object where they accurately reflect all tasks.
pub async fn dashboard(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let start = SERVER_START.get_or_init(std::time::Instant::now);
    let uptime_secs = start.elapsed().as_secs();

    let tq = &state.concurrency.task_queue;

    // Global done/failed counts from the in-memory cache (all projects combined).
    let all_tasks = state.core.tasks.list_all();
    let global_done: u64 = all_tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Done))
        .count() as u64;
    let global_failed: u64 = all_tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Failed))
        .count() as u64;

    // Most recent completed task with a PR URL, queried from the DB which is
    // ordered by created_at DESC — deterministic and newest-first.
    let latest_pr: Option<String> = state.core.tasks.latest_done_pr_url().await;

    // Grade from the global quality event store.
    let grade: Option<Value> = match state
        .observability
        .events
        .query(&EventFilters::default())
        .await
    {
        Ok(events) => {
            let report = QualityGrader::grade(&events, 0);
            serde_json::to_value(report.grade).ok()
        }
        Err(e) => {
            tracing::warn!("dashboard: failed to query events for grade: {e}");
            None
        }
    };

    // Build per-project entries from the registry.
    // Only live queue stats are included; done/failed/latest_pr/grade are not
    // available per-project (TaskState has no project_root field).
    let projects: Vec<Value> = match state.core.project_registry.as_ref() {
        None => vec![],
        Some(registry) => match registry.list().await {
            Err(e) => {
                tracing::warn!("dashboard: failed to list projects: {e}");
                vec![]
            }
            Ok(projects) => projects
                .into_iter()
                .map(|p| {
                    // Task queue keys are canonical project root paths as strings.
                    let key = p.root.to_string_lossy().into_owned();
                    let qs = tq.project_stats(&key);
                    json!({
                        "id": p.id,
                        "root": p.root,
                        "tasks": {
                            "running": qs.running,
                            "queued": qs.queued,
                        },
                    })
                })
                .collect(),
        },
    };

    let body = json!({
        "projects": projects,
        "global": {
            "running": tq.running_count(),
            "queued": tq.queued_count(),
            "max_concurrent": tq.global_limit(),
            "uptime_secs": uptime_secs,
            "done": global_done,
            "failed": global_failed,
            "latest_pr": latest_pr,
            "grade": grade,
        }
    });

    (StatusCode::OK, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
    use axum::{body::to_bytes, routing::get, Router};
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use std::sync::Arc;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let mut config = HarnessConfig::default();
        config.server.project_root = dir.to_path_buf();
        config.server.data_dir = dir.to_path_buf();
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        build_app_state(server).await
    }

    #[tokio::test]
    async fn dashboard_returns_ok_with_expected_shape() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-")?;
        let state = make_test_state(dir.path()).await?;
        let state = Arc::new(state);

        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/dashboard")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        // Must have top-level "projects" array and "global" object.
        assert!(body.get("projects").and_then(|v| v.as_array()).is_some());
        let global = body.get("global").expect("missing global key");
        assert!(global.get("running").is_some());
        assert!(global.get("queued").is_some());
        assert!(global.get("max_concurrent").is_some());
        assert!(global.get("uptime_secs").is_some());
        assert!(global.get("done").is_some());
        assert!(global.get("failed").is_some());

        Ok(())
    }

    #[tokio::test]
    async fn dashboard_global_fields_are_numeric() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-")?;
        let state = Arc::new(make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/dashboard")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;
        let global = &body["global"];

        assert!(global["running"].is_number(), "running must be a number");
        assert!(global["queued"].is_number(), "queued must be a number");
        assert!(
            global["max_concurrent"].is_number(),
            "max_concurrent must be a number"
        );
        assert!(
            global["uptime_secs"].is_number(),
            "uptime_secs must be a number"
        );

        Ok(())
    }
}
