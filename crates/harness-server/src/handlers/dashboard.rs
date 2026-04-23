use crate::{
    http::AppState,
    task_runner::{DashboardCounts, TaskStatus, TaskSummary},
};
use axum::{extract::State, http::StatusCode, Json};
use harness_core::types::{Event, EventFilters};
use harness_observe::{quality::QualityGrader, stats};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Rolling window for dashboard event queries. Keeps each `/api/dashboard` poll
/// from doing a full-history scan — the browser polls every 5 s and the event
/// store materialises all matching rows into memory on every call.
const DASHBOARD_EVENT_WINDOW_DAYS: i64 = 30;

/// Server start time. Initialized once in `serve()` before accepting connections,
/// so `uptime_secs` reflects true server uptime rather than time since first dashboard hit.
pub(crate) static SERVER_START: std::sync::OnceLock<std::time::Instant> =
    std::sync::OnceLock::new();

#[derive(Debug, Default, Serialize)]
struct DashboardFunnel {
    project_registered_at: Option<String>,
    task_submitted_at: Option<String>,
    live_output_at: Option<String>,
    completion_evidence_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct DashboardOnboarding {
    phase: &'static str,
    has_registered_project: bool,
    has_submitted_task: bool,
    has_live_output: bool,
    has_completion_evidence: bool,
}

fn record_first_timestamp(slot: &mut Option<String>, ts: &chrono::DateTime<chrono::Utc>) {
    let ts = ts.to_rfc3339();
    if slot.as_ref().is_none_or(|existing| ts < *existing) {
        *slot = Some(ts);
    }
}

fn parse_operator_funnel_milestone(event: &Event) -> Option<String> {
    if event.hook != "operator_funnel" {
        return None;
    }
    event.content.as_deref().and_then(|content| {
        serde_json::from_str::<serde_json::Value>(content)
            .ok()
            .and_then(|value| {
                value
                    .get("milestone")
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
            })
    })
}

fn task_has_started(summary: &TaskSummary) -> bool {
    matches!(
        summary.status,
        TaskStatus::Implementing
            | TaskStatus::AgentReview
            | TaskStatus::Waiting
            | TaskStatus::Reviewing
            | TaskStatus::Done
            | TaskStatus::Failed
    ) || summary.turn > 0
}

async fn build_onboarding_summary(
    state: &AppState,
    dashboard_events: &[Event],
) -> (bool, DashboardOnboarding, DashboardFunnel) {
    let mut funnel = DashboardFunnel::default();
    for event in dashboard_events {
        match parse_operator_funnel_milestone(event).as_deref() {
            Some("project_registered") => {
                record_first_timestamp(&mut funnel.project_registered_at, &event.ts)
            }
            Some("task_submitted") => {
                record_first_timestamp(&mut funnel.task_submitted_at, &event.ts)
            }
            Some("live_output_available") => {
                record_first_timestamp(&mut funnel.live_output_at, &event.ts)
            }
            Some("completion_evidence_available") => {
                record_first_timestamp(&mut funnel.completion_evidence_at, &event.ts)
            }
            _ => {}
        }
    }

    let task_summaries = match state.core.tasks.list_all_summaries_with_terminal().await {
        Ok(summaries) => summaries,
        Err(e) => {
            tracing::warn!("dashboard: failed to list task summaries for onboarding: {e}");
            Vec::new()
        }
    };

    if funnel.live_output_at.is_none() {
        for summary in task_summaries
            .iter()
            .filter(|summary| task_has_started(summary))
        {
            if let Some(created_at) = &summary.created_at {
                if funnel
                    .live_output_at
                    .as_ref()
                    .is_none_or(|existing| created_at < existing)
                {
                    funnel.live_output_at = Some(created_at.clone());
                }
            }
        }
    }

    let has_registered_project = funnel.project_registered_at.is_some();
    let has_submitted_task = funnel.task_submitted_at.is_some();
    let has_live_output = funnel.live_output_at.is_some();
    let has_completion_evidence = funnel.completion_evidence_at.is_some();

    let phase = if !has_registered_project {
        "register_project"
    } else if !has_submitted_task {
        "submit_task"
    } else if !has_live_output {
        "watch_live_output"
    } else if !has_completion_evidence {
        "inspect_completion"
    } else {
        "complete"
    };

    (
        phase != "complete",
        DashboardOnboarding {
            phase,
            has_registered_project,
            has_submitted_task,
            has_live_output,
            has_completion_evidence,
        },
        funnel,
    )
}

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
    } = state.task_svc.count_for_dashboard().await;

    // Most recent completed task with a PR URL, queried from the DB which is
    // ordered by updated_at DESC — reflects completion time, not creation time.
    let latest_pr: Option<String> = state.task_svc.latest_done_pr_url().await;

    // Grade from the global quality event store.
    // Derive violation_count from the most recent rule_scan session so we don't
    // permanently depress the grade with historical violations from old scans.
    let events_since = chrono::Utc::now() - chrono::Duration::days(DASHBOARD_EVENT_WINDOW_DAYS);
    let dashboard_events = match state
        .observability
        .events
        .query(&EventFilters {
            since: Some(events_since),
            ..Default::default()
        })
        .await
    {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!("dashboard: failed to query events: {e}");
            Vec::new()
        }
    };

    let grade: Option<Value> = {
        let events = &dashboard_events;
        if events.is_empty() {
            None
        } else {
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
                let report = QualityGrader::grade(events, violation_count);
                serde_json::to_value(report.grade).ok()
            }
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
    let projects: Vec<Value> = match state.project_svc.list().await {
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
    };

    let runtime_hosts: Vec<Value> = state
        .runtime_hosts
        .list_hosts()
        .into_iter()
        .map(|host| {
            let snapshot = state.runtime_project_cache.get_host_cache(&host.id);
            let watched_projects = snapshot.as_ref().map(|s| s.project_count).unwrap_or(0);
            let watched_project_roots: Vec<&str> = snapshot
                .as_ref()
                .map(|s| s.projects.iter().map(|p| p.root.as_str()).collect())
                .unwrap_or_default();
            let active_leases = state.runtime_hosts.active_lease_count(&host.id);
            let assignment_pressure = active_leases as f64 / watched_projects.max(1) as f64;
            json!({
                "id": host.id,
                "display_name": host.display_name,
                "capabilities": host.capabilities,
                "online": host.online,
                "last_heartbeat_at": host.last_heartbeat_at,
                "watched_projects": watched_projects,
                "watched_project_roots": watched_project_roots,
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
    let (first_run, onboarding, funnel) = build_onboarding_summary(&state, &dashboard_events).await;

    let body = json!({
        "projects": projects,
        "runtime_hosts": runtime_hosts,
        "llm_metrics": llm_metrics,
        "first_run": first_run,
        "onboarding": onboarding,
        "funnel": funnel,
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
        assert!(body.get("first_run").is_some());
        assert!(body.get("onboarding").is_some());
        assert!(body.get("funnel").is_some());
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

    #[tokio::test]
    async fn dashboard_aggregates_operator_funnel_milestones() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = crate::test_helpers::tempdir_in_home("harness-test-dashboard-")?;
        let state = Arc::new(make_test_state(dir.path()).await?);

        for milestone in [
            "project_registered",
            "task_submitted",
            "live_output_available",
            "completion_evidence_available",
        ] {
            let mut event = harness_core::types::Event::new(
                harness_core::types::SessionId::new(),
                "operator_funnel",
                "test",
                harness_core::types::Decision::Complete,
            );
            event.content = Some(serde_json::json!({ "milestone": milestone }).to_string());
            state.observability.events.log(&event).await?;
        }

        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/dashboard")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert_eq!(body["first_run"], false);
        assert_eq!(body["onboarding"]["phase"], "complete");
        assert_eq!(body["onboarding"]["has_registered_project"], true);
        assert_eq!(body["onboarding"]["has_submitted_task"], true);
        assert_eq!(body["onboarding"]["has_live_output"], true);
        assert_eq!(body["onboarding"]["has_completion_evidence"], true);
        assert!(body["funnel"]["project_registered_at"].is_string());
        assert!(body["funnel"]["task_submitted_at"].is_string());
        assert!(body["funnel"]["live_output_at"].is_string());
        assert!(body["funnel"]["completion_evidence_at"].is_string());
        Ok(())
    }
}
