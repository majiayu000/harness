use crate::{http::AppState, task_runner::DashboardCounts};
use axum::{extract::State, http::StatusCode, Json};
use harness_core::types::EventFilters;
use harness_observe::{quality::QualityGrader, stats};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc};

use super::dashboard_active_counts::{dashboard_active_counts, DashboardProjectActiveCounts};

/// Rolling window for dashboard event queries. Keeps each `/api/dashboard` poll
/// from doing a full-history scan — the browser polls every 5 s and the event
/// store materialises all matching rows into memory on every call.
const DASHBOARD_EVENT_WINDOW_DAYS: i64 = 30;

/// Server start time. Initialized once in `serve()` before accepting connections,
/// so `uptime_secs` reflects true server uptime rather than time since first dashboard hit.
pub(crate) static SERVER_START: std::sync::OnceLock<std::time::Instant> =
    std::sync::OnceLock::new();

/// GET /api/dashboard — JSON summary of all registered projects and global concurrency.
///
/// Per-project entries include active runtime/legacy work stats
/// (running/queued) plus historical done/failed counts and the latest PR URL
/// for the project. Per-host entries include active lease count and assignment
/// pressure (active_leases / max(watched_projects, 1)).
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
        global_stalled,
        by_project: project_counts,
    } = state.task_svc.count_for_dashboard().await;

    // Most recent completed task with a PR URL, queried from the DB which is
    // ordered by updated_at DESC — reflects completion time, not creation time.
    let latest_pr: Option<String> = state.task_svc.latest_done_pr_url().await;
    let project_list = state.project_svc.list().await;
    let visible_project_ids = project_list.as_ref().ok().map(|projects| {
        projects
            .iter()
            .map(|project| project.root.to_string_lossy().into_owned())
            .collect::<HashSet<_>>()
    });
    let active_counts = dashboard_active_counts(&state, visible_project_ids.as_ref()).await;

    // Grade from the global quality event store.
    // Derive violation_count from the most recent rule_scan session so we don't
    // permanently depress the grade with historical violations from old scans.
    let events_since = chrono::Utc::now() - chrono::Duration::days(DASHBOARD_EVENT_WINDOW_DAYS);
    let grade: Option<Value> = match state
        .observability
        .events
        .query(&EventFilters {
            since: Some(events_since),
            ..Default::default()
        })
        .await
    {
        Ok(events) => {
            // Return None when the rolling window contains no events rather than
            // computing a false perfect grade from an empty set.  Operators should
            // see null (unknown) instead of an inflated A when the project has been
            // quiet for more than DASHBOARD_EVENT_WINDOW_DAYS days.
            if events.iter().all(|e| e.hook != "rule_scan") {
                None
            } else {
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
        }
        Err(e) => {
            tracing::warn!("dashboard: failed to query events for grade: {e}");
            None
        }
    };

    // LLM-specific metrics derived from in-memory task state and event store.
    //
    // collect_llm_metrics_inputs() avoids two pitfalls:
    // 1. Performance: does NOT call list_all() which would clone full TaskState
    //    values (including large rounds.detail strings) on every 5-second poll.
    //    Turn counts come from lightweight DB summaries; latencies are read
    //    in-place from cache refs without any full-TaskState allocation.
    // 2. Correctness: turn counts include terminal tasks from the DB so the
    //    metrics are not zero when the queue is idle.  Latency computation skips
    //    the synthetic "resumed_checkpoint" round injected at recovery time so
    //    resumed tasks are not silently excluded from the p50.
    let llm_metrics: Value = {
        let inputs = state.core.tasks.collect_llm_metrics_inputs().await;
        let turn_counts = inputs.turn_counts;
        let avg_turns: Option<f64> = if turn_counts.is_empty() {
            None
        } else {
            Some(turn_counts.iter().map(|&t| t as f64).sum::<f64>() / turn_counts.len() as f64)
        };
        let p50_turns = stats::p50_turns(&turn_counts);
        let mut latencies = inputs.first_token_latencies;
        latencies.sort_unstable();
        let p50_first_token_latency_ms: Option<u64> = if latencies.is_empty() {
            None
        } else {
            // Lower-median: index (len-1)/2 avoids overstating p50 for even-length slices.
            Some(latencies[(latencies.len() - 1) / 2])
        };
        // total_linter_feedback from events already queried above.
        // Bounded to the same rolling window as the grade query to avoid a
        // full-history scan on every 5-second dashboard poll.
        let total_linter_feedback = match state
            .observability
            .events
            .query(&EventFilters {
                hook: Some("rule_check".to_string()),
                since: Some(events_since),
                ..Default::default()
            })
            .await
        {
            Ok(evts) => stats::linter_feedback_count(&evts),
            Err(e) => {
                tracing::warn!("dashboard: failed to query rule_check events: {e}");
                0
            }
        };
        json!({
            "avg_turns": avg_turns,
            "p50_turns": p50_turns,
            "total_linter_feedback": total_linter_feedback,
            "p50_first_token_latency_ms": p50_first_token_latency_ms,
        })
    };

    // Fetch latest PR URLs for all projects in one bulk query to avoid N+1.
    let project_pr_urls = state.task_svc.latest_done_pr_urls_all_projects().await;

    // Build per-project entries from the registry.
    let projects: Vec<Value> = match project_list {
        Err(e) => {
            tracing::warn!("dashboard: failed to list projects: {e}");
            vec![]
        }
        Ok(projects) => {
            let mut entries = Vec::with_capacity(projects.len());
            for p in projects {
                let key = p.root.to_string_lossy().into_owned();
                let active_project_counts = if active_counts.used_task_queue_fallback {
                    let stats = tq.project_stats(&key);
                    DashboardProjectActiveCounts {
                        running: stats.running,
                        queued: stats.queued,
                    }
                } else {
                    active_counts
                        .by_project
                        .get(&key)
                        .copied()
                        .unwrap_or_default()
                };
                let counts = project_counts.get(&key);
                let done = counts.map_or(0, |c| c.done);
                let failed = counts.map_or(0, |c| c.failed);
                let stalled = counts.map_or(0, |c| c.stalled);
                let latest_pr = project_pr_urls.get(&key);
                entries.push(json!({
                    "id": p.id,
                    "root": p.root,
                    "tasks": {
                        "running": active_project_counts.running,
                        "queued": active_project_counts.queued,
                        "done": done,
                        "failed": failed,
                        "stalled": stalled,
                    },
                    "latest_pr": latest_pr,
                }));
            }
            entries
        }
    };

    let mut runtime_hosts = Vec::new();
    for host in state.runtime_hosts.list_hosts() {
        let snapshot = state.runtime_project_cache.get_host_cache(&host.id);
        let watched_projects = snapshot.as_ref().map(|s| s.project_count).unwrap_or(0);
        let watched_project_roots: Vec<&str> = snapshot
            .as_ref()
            .map(|s| s.projects.iter().map(|p| p.root.as_str()).collect())
            .unwrap_or_default();
        let active_leases =
            match super::runtime_hosts::active_runtime_job_lease_count(&state, &host.id).await {
                Ok(count) => Some(count),
                Err(error) => {
                    tracing::warn!(
                        host_id = %host.id,
                        "dashboard: failed to count runtime-job leases: {error}"
                    );
                    None
                }
            };
        let assignment_pressure =
            active_leases.map(|count| count as f64 / watched_projects.max(1) as f64);
        runtime_hosts.push(json!({
            "id": host.id,
            "display_name": host.display_name,
            "capabilities": host.capabilities,
            "online": host.online,
            "last_heartbeat_at": host.last_heartbeat_at,
            "watched_projects": watched_projects,
            "watched_project_roots": watched_project_roots,
            "active_leases": active_leases,
            "assignment_pressure": assignment_pressure,
        }));
    }
    let runtime_hosts_total = runtime_hosts.len() as u64;
    let runtime_hosts_online = runtime_hosts
        .iter()
        .filter(|host| host["online"].as_bool().unwrap_or(false))
        .count() as u64;

    let body = json!({
        "projects": projects,
        "runtime_hosts": runtime_hosts,
        "llm_metrics": llm_metrics,
        "global": {
            "running": active_counts.running,
            "queued": active_counts.queued,
            "max_concurrent": tq.global_limit(),
            "uptime_secs": uptime_secs,
            "done": global_done,
            "failed": global_failed,
            "stalled": global_stalled,
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
    use crate::task_runner::{TaskId, TaskState};
    use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
    use axum::{body::to_bytes, routing::get, Router};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;
    use std::sync::Arc;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        let mut config = HarnessConfig::default();
        config.server.project_root = dir.to_path_buf();
        config.server.data_dir = dir.to_path_buf();
        make_test_state_with_config(config).await
    }

    async fn make_test_state_with_config(mut config: HarnessConfig) -> anyhow::Result<AppState> {
        config.server.allow_unauthenticated = true;
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        build_app_state(server).await
    }

    async fn dashboard_body(state: Arc<AppState>) -> anyhow::Result<serde_json::Value> {
        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .with_state(state);
        let req = axum::http::Request::builder()
            .uri("/api/dashboard")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn project_entry<'a>(
        body: &'a serde_json::Value,
        project_root: &str,
    ) -> anyhow::Result<&'a serde_json::Value> {
        body["projects"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("projects should be an array"))?
            .iter()
            .find(|project| project["root"].as_str() == Some(project_root))
            .ok_or_else(|| anyhow::anyhow!("project should be present in dashboard"))
    }

    fn runtime_workflow(
        state: &str,
        project_id: &str,
        task_id: &str,
    ) -> harness_workflow::runtime::WorkflowInstance {
        harness_workflow::runtime::WorkflowInstance::new(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            state,
            harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1371"),
        )
        .with_data(serde_json::json!({
            "project_id": project_id,
            "task_id": task_id,
        }))
    }

    #[tokio::test]
    async fn dashboard_counts_runtime_only_active_workflow() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-runtime-active-")?;
        let project_root = dir.path().canonicalize()?.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workflow runtime store should be configured"))?;
        store
            .upsert_instance(&runtime_workflow(
                "implementing",
                &project_root,
                "runtime-dashboard-active",
            ))
            .await?;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 1);
        assert_eq!(body["global"]["queued"], 0);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 1);
        assert_eq!(project["tasks"]["queued"], 0);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_ignores_terminal_runtime_workflow_for_active_counts() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-runtime-terminal-")?;
        let project_root = dir.path().canonicalize()?.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workflow runtime store should be configured"))?;
        store
            .upsert_instance(&runtime_workflow(
                "done",
                &project_root,
                "runtime-dashboard-terminal",
            ))
            .await?;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 0);
        assert_eq!(body["global"]["queued"], 0);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 0);
        assert_eq!(project["tasks"]["queued"], 0);
        Ok(())
    }

    #[tokio::test]
    async fn status_stalled_terminal_dashboard_counts_budget_exhaustion() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-stalled-")?;
        let canonical_root = dir.path().canonicalize()?;
        let project_root = canonical_root.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let mut task = TaskState::new(TaskId::from_str("dashboard-stalled-task"));
        task.project_root = Some(canonical_root);
        task.status = crate::task_runner::TaskStatus::Failed;
        task.phase = crate::task_runner::TaskPhase::Terminal;
        task.error = Some(
            crate::task_runner::TaskTerminalFailure::round_budget_exhausted(
                1,
                crate::task_runner::TaskStatus::Reviewing,
                Some("local_review_gate".to_string()),
            )
            .to_reason_string(),
        );
        task.scheduler
            .mark_terminal(&crate::task_runner::TaskStatus::Failed);
        state.core.tasks.insert(&task).await;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["failed"], 1);
        assert_eq!(body["global"]["stalled"], 1);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 0);
        assert_eq!(project["tasks"]["queued"], 0);
        assert_eq!(project["tasks"]["failed"], 1);
        assert_eq!(project["tasks"]["stalled"], 1);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_dedupes_runtime_workflow_with_active_legacy_task() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-runtime-dedupe-")?;
        let canonical_root = dir.path().canonicalize()?;
        let project_root = canonical_root.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let task_id = TaskId::from_str("legacy-dashboard-active");
        let mut task = TaskState::new(task_id.clone());
        task.project_root = Some(canonical_root);
        state.core.tasks.insert(&task).await;

        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("workflow runtime store should be configured"))?;
        store
            .upsert_instance(&runtime_workflow(
                "implementing",
                &project_root,
                task_id.as_str(),
            ))
            .await?;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 0);
        assert_eq!(body["global"]["queued"], 1);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 0);
        assert_eq!(project["tasks"]["queued"], 1);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_counts_recovering_legacy_task_as_running() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-recovering-task-")?;
        let canonical_root = dir.path().canonicalize()?;
        let project_root = canonical_root.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);

        let mut task = TaskState::new(TaskId::from_str("legacy-dashboard-recovering"));
        task.project_root = Some(canonical_root);
        task.scheduler.mark_recovering("test-scheduler");
        state.core.tasks.insert(&task).await;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 1);
        assert_eq!(body["global"]["queued"], 0);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 1);
        assert_eq!(project["tasks"]["queued"], 0);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_counts_unregistered_project_work_in_global_totals() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir =
            crate::test_helpers::tempdir_in_home("harness-test-dashboard-unregistered-task-")?;
        let registered_root = dir.path().canonicalize()?.to_string_lossy().into_owned();
        let unregistered_dir = dir.path().join("unregistered-project");
        std::fs::create_dir_all(&unregistered_dir)?;
        let unregistered_root = unregistered_dir
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);

        let mut task = TaskState::new(TaskId::from_str("legacy-dashboard-unregistered"));
        task.project_root = Some(unregistered_dir.canonicalize()?);
        task.scheduler.claim_scheduler("test-scheduler");
        state.core.tasks.insert(&task).await;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 1);
        assert_eq!(body["global"]["queued"], 0);
        let registered_project = project_entry(&body, &registered_root)?;
        assert_eq!(registered_project["tasks"]["running"], 0);
        assert_eq!(registered_project["tasks"]["queued"], 0);
        assert!(
            project_entry(&body, &unregistered_root).is_err(),
            "unregistered project must not appear in the project table"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_counts_unregistered_allowed_root_work_in_global_totals() -> anyhow::Result<()>
    {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let default_dir =
            crate::test_helpers::tempdir_in_home("harness-test-dashboard-allowed-default-")?;
        let allowed_dir =
            crate::test_helpers::tempdir_in_home("harness-test-dashboard-allowed-root-")?;
        let registered_root = default_dir
            .path()
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let unregistered_dir = allowed_dir.path().join("unregistered-project");
        std::fs::create_dir_all(&unregistered_dir)?;
        let unregistered_root = unregistered_dir
            .canonicalize()?
            .to_string_lossy()
            .into_owned();

        let mut config = HarnessConfig::default();
        config.server.project_root = default_dir.path().to_path_buf();
        config.server.data_dir = default_dir.path().to_path_buf();
        config.server.allowed_project_roots = vec![allowed_dir.path().to_path_buf()];
        let state = Arc::new(make_test_state_with_config(config).await?);

        let mut task = TaskState::new(TaskId::from_str("legacy-dashboard-allowed-root"));
        task.project_root = Some(unregistered_dir.canonicalize()?);
        task.scheduler.claim_scheduler("test-scheduler");
        state.core.tasks.insert(&task).await;

        let body = dashboard_body(state).await?;

        assert_eq!(body["global"]["running"], 1);
        assert_eq!(body["global"]["queued"], 0);
        let registered_project = project_entry(&body, &registered_root)?;
        assert_eq!(registered_project["tasks"]["running"], 0);
        assert_eq!(registered_project["tasks"]["queued"], 0);
        assert!(
            project_entry(&body, &unregistered_root).is_err(),
            "unregistered allowed project must not appear in the project table"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_preserves_foreground_task_queue_waiters() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-queue-waiter-")?;
        let canonical_root = dir.path().canonicalize()?;
        let project_root = canonical_root.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let queue = state.concurrency.task_queue.clone();
        queue.set_project_limit(&project_root, 1);
        let holder = queue.acquire(&project_root, 0).await?;

        let waiter_queue = queue.clone();
        let waiter_project = project_root.clone();
        let waiter = tokio::spawn(async move { waiter_queue.acquire(&waiter_project, 0).await });
        for _ in 0..20 {
            if queue.project_stats(&project_root).queued == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(queue.project_stats(&project_root).queued, 1);

        let body = dashboard_body(state).await?;

        waiter.abort();
        drop(holder);
        let join_error = waiter
            .await
            .expect_err("aborted queue waiter should return a join error");
        assert!(join_error.is_cancelled());

        assert_eq!(body["global"]["running"], 0);
        assert_eq!(body["global"]["queued"], 1);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 0);
        assert_eq!(project["tasks"]["queued"], 1);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_does_not_double_count_background_queue_waiters() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir =
            crate::test_helpers::tempdir_in_home("harness-test-dashboard-background-waiter-")?;
        let canonical_root = dir.path().canonicalize()?;
        let project_root = canonical_root.to_string_lossy().into_owned();
        let state = Arc::new(make_test_state(dir.path()).await?);
        let queue = state.concurrency.task_queue.clone();
        queue.set_project_limit(&project_root, 1);
        let holder = queue.acquire(&project_root, 0).await?;

        let mut task = TaskState::new(TaskId::from_str("legacy-dashboard-background-waiter"));
        task.project_root = Some(canonical_root);
        state.core.tasks.insert(&task).await;

        let waiter_queue = queue.clone();
        let waiter_project = project_root.clone();
        let waiter = tokio::spawn(async move { waiter_queue.acquire(&waiter_project, 0).await });
        for _ in 0..20 {
            if queue.project_stats(&project_root).queued == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(queue.project_stats(&project_root).queued, 1);

        let body = dashboard_body(state).await?;

        waiter.abort();
        drop(holder);
        let join_error = waiter
            .await
            .expect_err("aborted queue waiter should return a join error");
        assert!(join_error.is_cancelled());

        assert_eq!(body["global"]["running"], 0);
        assert_eq!(body["global"]["queued"], 1);
        let project = project_entry(&body, &project_root)?;
        assert_eq!(project["tasks"]["running"], 0);
        assert_eq!(project["tasks"]["queued"], 1);
        Ok(())
    }

    #[tokio::test]
    async fn dashboard_returns_ok_with_expected_shape() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
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
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
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
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
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
