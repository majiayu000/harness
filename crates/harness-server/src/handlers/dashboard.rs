use crate::{http::AppState, task_runner::DashboardCounts};
use axum::{extract::State, http::StatusCode, Json};
use harness_core::types::EventFilters;
use harness_observe::quality::QualityGrader;
use serde_json::{json, Value};
use std::sync::Arc;

/// Server start time. Initialized once in `serve()` before accepting connections,
/// so `uptime_secs` reflects true server uptime rather than time since first dashboard hit.
pub(crate) static SERVER_START: std::sync::OnceLock<std::time::Instant> =
    std::sync::OnceLock::new();

/// GET /api/dashboard — JSON summary of all registered projects and global concurrency.
///
/// Per-project entries include live queue stats (running/queued) plus historical
/// done/failed counts from the in-memory cache (keyed by `TaskState.project_root`)
/// and the latest PR URL for the project. Per-host entries include active lease
/// count and assignment pressure (active_leases / max(watched_projects, 1)).
/// Global metrics (done, failed, latest_pr, grade, uptime) aggregate all tasks.
pub async fn dashboard(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let start = SERVER_START.get_or_init(std::time::Instant::now);
    let uptime_secs = start.elapsed().as_secs();

    let tq = &state.concurrency.task_queue;

    // Global and per-project done/failed counts from in-memory cache in one pass,
    // avoiding both full TaskState cloning and double iteration.
    let DashboardCounts {
        global_done,
        global_failed,
        by_project: project_counts,
    } = state.core.tasks.count_for_dashboard();

    // Most recent completed task with a PR URL, queried from the DB which is
    // ordered by updated_at DESC — reflects completion time, not creation time.
    let latest_pr: Option<String> = state.core.tasks.latest_done_pr_url().await;

    // Grade from the global quality event store.
    // Derive violation_count from the most recent rule_scan session so we don't
    // permanently depress the grade with historical violations from old scans.
    let grade: Option<Value> = match state
        .observability
        .events
        .query(&EventFilters::default())
        .await
    {
        Ok(events) => {
            let violation_count = events
                .iter()
                .rev()
                .find(|e| e.hook == "rule_scan")
                .map(|scan| {
                    events
                        .iter()
                        .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
                        .count()
                })
                .unwrap_or(0);
            let report = QualityGrader::grade(&events, violation_count);
            serde_json::to_value(report.grade).ok()
        }
        Err(e) => {
            tracing::warn!("dashboard: failed to query events for grade: {e}");
            None
        }
    };

    // Fetch latest PR URLs for all projects in one bulk query to avoid N+1.
    let project_pr_urls = state.core.tasks.latest_done_pr_urls_all_projects().await;

    // Build per-project entries from the registry.
    let projects: Vec<Value> = match state.core.project_registry.as_ref() {
        None => vec![],
        Some(registry) => match registry.list().await {
            Err(e) => {
                tracing::warn!("dashboard: failed to list projects: {e}");
                vec![]
            }
            Ok(projects) => {
                let mut entries = Vec::with_capacity(projects.len());
                for p in projects {
                    // Task queue keys are canonical project root paths as strings.
                    let key = p.root.to_string_lossy().into_owned();
                    let qs = tq.project_stats(&key);
                    let counts = project_counts.get(&key);
                    let done = counts.map_or(0, |c| c.done);
                    let failed = counts.map_or(0, |c| c.failed);
                    let latest_pr = project_pr_urls.get(&key);
                    entries.push(json!({
                        "id": p.id,
                        "root": p.root,
                        "tasks": {
                            "running": qs.running,
                            "queued": qs.queued,
                            "done": done,
                            "failed": failed,
                        },
                        "latest_pr": latest_pr,
                    }));
                }
                entries
            }
        },
    };

    let runtime_hosts: Vec<Value> = state
        .runtime_hosts
        .list_hosts()
        .into_iter()
        .map(|host| {
            let watched_projects = state
                .runtime_project_cache
                .get_host_cache(&host.id)
                .map(|snapshot| snapshot.project_count)
                .unwrap_or(0);
            let active_leases = state.runtime_hosts.active_lease_count(&host.id);
            let assignment_pressure = active_leases as f64 / watched_projects.max(1) as f64;
            json!({
                "id": host.id,
                "display_name": host.display_name,
                "capabilities": host.capabilities,
                "online": host.online,
                "last_heartbeat_at": host.last_heartbeat_at,
                "watched_projects": watched_projects,
                "active_leases": active_leases,
                "assignment_pressure": assignment_pressure,
            })
        })
        .collect();
    let runtime_hosts_total = runtime_hosts.len() as u64;
    let runtime_hosts_online = runtime_hosts
        .iter()
        .filter(|host| host["online"].as_bool().unwrap_or(false))
        .count() as u64;

    let body = json!({
        "projects": projects,
        "runtime_hosts": runtime_hosts,
        "global": {
            "running": tq.running_count(),
            "queued": tq.queued_count(),
            "max_concurrent": tq.global_limit(),
            "uptime_secs": uptime_secs,
            "done": global_done,
            "failed": global_failed,
            "latest_pr": latest_pr,
            "grade": grade,
            "runtime_hosts_total": runtime_hosts_total,
            "runtime_hosts_online": runtime_hosts_online,
        }
    });

    (StatusCode::OK, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
    use axum::{body::to_bytes, routing::get, Router};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;
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
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
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
        assert!(global.get("runtime_hosts_total").is_some());
        assert!(global.get("runtime_hosts_online").is_some());
        assert!(body
            .get("runtime_hosts")
            .and_then(|v| v.as_array())
            .is_some());

        Ok(())
    }

    #[tokio::test]
    async fn dashboard_global_fields_are_numeric() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
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

    #[tokio::test]
    async fn dashboard_host_assignment_pressure_zero_when_idle() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-")?;
        let state = make_test_state(dir.path()).await?;

        // Register a host with no leases.
        state
            .runtime_hosts
            .register("idle-host".to_string(), None, vec![]);

        let state = Arc::new(state);
        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/dashboard")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        let hosts = body["runtime_hosts"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("runtime_hosts must be an array"))?;
        let host = hosts
            .iter()
            .find(|h| h["id"] == "idle-host")
            .ok_or_else(|| anyhow::anyhow!("idle-host not found in runtime_hosts"))?;
        assert_eq!(
            host["active_leases"], 0,
            "idle host must have 0 active_leases"
        );
        assert_eq!(
            host["assignment_pressure"], 0.0,
            "idle host must have 0.0 assignment_pressure"
        );
        Ok(())
    }
}
