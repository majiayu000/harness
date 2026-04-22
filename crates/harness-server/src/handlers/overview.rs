//! GET /api/overview — system-level aggregated view across all projects.
//!
//! Feeds the `/overview` HTML page (served from `crate::overview::index`).
//! Aggregates data from existing services rather than introducing new
//! tracking: task counts come from `TaskService`, runtime fleet state from
//! `RuntimeHostManager`, quality grade and rule-fail rate from the event
//! store. Metrics that harness does not yet track (per-agent tokens, runtime
//! CPU/RAM) are returned as `null` so the UI degrades gracefully.

use crate::http::AppState;
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Duration, Timelike, Utc};
use harness_core::types::{Decision, Event, EventFilters};
use harness_observe::quality::QualityGrader;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Rolling window for the overview page. Matches the `24h` segment pill in
/// the design; other windows are a client-side concern.
const OVERVIEW_WINDOW_HOURS: i64 = 24;

/// Number of hourly buckets for the throughput chart and per-project trend.
const THROUGHPUT_BUCKETS: usize = 24;

/// Number of feed items returned. The design's mock feed shows ~14 rows with
/// scroll; capping at 40 keeps the payload bounded without starving the UI.
const FEED_LIMIT: usize = 40;

/// GET /api/overview — JSON payload driving the system overview page.
pub async fn overview(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let now = Utc::now();
    // Snap the query window to the start of the oldest bucket on the hour
    // axis so that SQL rows and JS buckets agree. Without this, tasks
    // completed in the partial "now - 24h .. oldest-bucket-start" slice are
    // counted in the global `merged_24h` KPI but dropped when their
    // `strftime('%H:00:00Z')` hour key falls outside the axis, which makes
    // per-project sums understate the KPI for every request that is not
    // exactly on the hour.
    let since_dt = window_start(now);

    // ---- global task queue counts (reuse existing services) ----
    let tq = &state.concurrency.task_queue;
    let running = tq.running_count();
    let queued = tq.queued_count();
    let max_concurrent = tq.global_limit();

    let dashboard_counts = state.task_svc.count_for_dashboard().await;
    let global_done = dashboard_counts.global_done;
    let global_failed = dashboard_counts.global_failed;

    let merged_24h = state.task_svc.count_done_since(since_dt).await;

    // Per-project hourly done counts — drives both the fleet throughput chart
    // and each project's 24h trend sparkline.
    let hour_rows = state.task_svc.done_per_project_hour_since(since_dt).await;
    let hours = build_hour_axis(now);
    let hour_index: HashMap<String, usize> = hours
        .iter()
        .enumerate()
        .map(|(i, h)| (h.clone(), i))
        .collect();
    let mut per_project_buckets: HashMap<String, Vec<u64>> = HashMap::new();
    for (project_key, hour, count) in hour_rows {
        if let Some(&idx) = hour_index.get(&hour) {
            let entry = per_project_buckets
                .entry(project_key)
                .or_insert_with(|| vec![0; THROUGHPUT_BUCKETS]);
            entry[idx] = entry[idx].saturating_add(count);
        }
    }

    // ---- projects table ----
    let project_pr_urls = state.task_svc.latest_done_pr_urls_all_projects().await;
    let project_list = state.project_svc.list().await.unwrap_or_else(|e| {
        tracing::warn!("overview: failed to list projects: {e}");
        Vec::new()
    });
    let mut projects_json: Vec<Value> = Vec::with_capacity(project_list.len());
    let mut throughput_series: Vec<Value> = Vec::with_capacity(project_list.len());
    for p in &project_list {
        let key = p.root.to_string_lossy().into_owned();
        let qs = tq.project_stats(&key);
        let counts = dashboard_counts.by_project.get(&key);
        let done = counts.map_or(0, |c| c.done);
        let failed = counts.map_or(0, |c| c.failed);
        let buckets = per_project_buckets
            .remove(&key)
            .unwrap_or_else(|| vec![0; THROUGHPUT_BUCKETS]);
        let merged_window: u64 = buckets.iter().sum();
        projects_json.push(json!({
            "id": p.id,
            "root": p.root,
            "running": qs.running,
            "queued": qs.queued,
            "done": done,
            "failed": failed,
            "merged_24h": merged_window,
            "trend": buckets,
            "avg_score": Value::Null,
            "worktrees": Value::Null,
            "tokens_24h": Value::Null,
            "agents": Vec::<String>::new(),
            "latest_pr": project_pr_urls.get(&key),
        }));
        throughput_series.push(json!({
            "project": p.id,
            "values": buckets,
        }));
    }

    // Any buckets left over after draining the registry belong to tasks
    // whose project is no longer registered (or was never registered).
    // Emit them as fallback series so the stacked-area sum still matches
    // the global `merged_24h` KPI — otherwise deleting a project while it
    // had recent done tasks would silently desync the chart from the KPI.
    let mut leftover: Vec<(String, Vec<u64>)> = per_project_buckets.drain().collect();
    leftover.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, values) in leftover {
        if !values.iter().any(|&v| v > 0) {
            continue;
        }
        let label = if key.is_empty() {
            "unassigned".to_string()
        } else {
            format!("unregistered · {key}")
        };
        throughput_series.push(json!({
            "project": label,
            "values": values,
        }));
    }

    // ---- runtime fleet ----
    let runtime_hosts = state.runtime_hosts.list_hosts();
    let mut runtimes_json: Vec<Value> = Vec::with_capacity(runtime_hosts.len());
    // Local single-host worktrees managed by WorkspaceManager, plus any leases
    // held by remote runtime hosts. Without the workspace_mgr term, single-host
    // deployments (no registered runtimes) always report 0.
    let local_worktrees = state
        .concurrency
        .workspace_mgr
        .as_ref()
        .map_or(0, |m| m.live_count());
    let mut worktrees_used: u64 = local_worktrees;
    for host in &runtime_hosts {
        let active_leases = state.runtime_hosts.active_lease_count(&host.id) as u64;
        worktrees_used = worktrees_used.saturating_add(active_leases);
        let watched_projects = state
            .runtime_project_cache
            .get_host_cache(&host.id)
            .map(|snapshot| snapshot.project_count)
            .unwrap_or(0);
        runtimes_json.push(json!({
            "id": host.id,
            "display_name": host.display_name,
            "capabilities": host.capabilities,
            "online": host.online,
            "last_heartbeat_at": host.last_heartbeat_at,
            "active_leases": active_leases,
            "watched_projects": watched_projects,
            "cpu_pct": Value::Null,
            "ram_pct": Value::Null,
            "tokens_24h": Value::Null,
        }));
    }

    // ---- event-derived metrics (grade, rule fail rate, feed, alerts) ----
    let events = match state
        .observability
        .events
        .query(&EventFilters {
            since: Some(since_dt),
            ..Default::default()
        })
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("overview: failed to query events: {e}");
            Vec::new()
        }
    };

    let (avg_review_score, grade_letter) = compute_grade(&events);
    let rule_fail_rate_pct = compute_rule_fail_rate_pct(&events);

    let feed = build_feed(&events, now);
    let alerts = build_alerts(&events, &runtime_hosts);

    // Cluster heatmap: per-project 24-bucket intensity (normalised to the
    // project's peak hour). Runtime-dimension is collapsed because task rows
    // don't carry host attribution yet.
    let heatmap_rows: Vec<Value> = projects_json
        .iter()
        .filter_map(|p| {
            let label = p["id"].as_str()?;
            let trend = p["trend"].as_array()?;
            let values: Vec<u64> = trend.iter().filter_map(|v| v.as_u64()).collect();
            let peak = values.iter().copied().max().unwrap_or(0);
            let intensity: Vec<f64> = if peak == 0 {
                values.iter().map(|_| 0.0).collect()
            } else {
                values.iter().map(|v| *v as f64 / peak as f64).collect()
            };
            Some(json!({ "label": label, "intensity": intensity }))
        })
        .collect();

    let uptime_secs = crate::handlers::dashboard::SERVER_START
        .get()
        .map(|s| s.elapsed().as_secs())
        .unwrap_or(0);

    let body = json!({
        "window": {
            "hours": OVERVIEW_WINDOW_HOURS,
            "since": since_dt.to_rfc3339(),
            "now": now.to_rfc3339(),
        },
        "kpi": {
            "active_tasks": (running as u64) + (queued as u64),
            "merged_24h": merged_24h,
            "avg_review_score": avg_review_score,
            "grade": grade_letter,
            "rule_fail_rate_pct": rule_fail_rate_pct,
            "tokens_24h": Value::Null,
            "worktrees": {
                "used": worktrees_used,
                "total": max_concurrent,
            },
        },
        "distribution": {
            "queued": queued,
            "running": running,
            "review": 0,
            "merged": global_done,
            "failed": global_failed,
        },
        "throughput": {
            "hours": hours,
            "series": throughput_series,
        },
        "projects": projects_json,
        "runtimes": runtimes_json,
        "heatmap": {
            "bucket_minutes": 60,
            "rows": heatmap_rows,
        },
        "feed": feed,
        "alerts": alerts,
        "global": {
            "uptime_secs": uptime_secs,
            "running": running,
            "queued": queued,
            "done": global_done,
            "failed": global_failed,
            "max_concurrent": max_concurrent,
        },
    });

    (StatusCode::OK, Json(body))
}

/// Start of the oldest bucket on the hour axis. Shared by the SQL `since`
/// filter and [`build_hour_axis`] so the two views align.
fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    hour_top(now) - Duration::hours((THROUGHPUT_BUCKETS as i64) - 1)
}

/// Top of the current hour (i.e. `HH:00:00Z`). Falls back to `now` on the
/// near-impossible DST-style failure of `with_minute(0)`.
fn hour_top(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now)
}

/// Build an ISO-8601 hour axis covering the last [`THROUGHPUT_BUCKETS`] hours
/// ending at `now`, oldest first. Each element matches the format produced by
/// `TaskDb::done_per_project_hour_since` so the two can be joined by string
/// equality.
fn build_hour_axis(now: DateTime<Utc>) -> Vec<String> {
    let top = hour_top(now);
    let mut out = Vec::with_capacity(THROUGHPUT_BUCKETS);
    for i in (0..THROUGHPUT_BUCKETS).rev() {
        let ts = top - Duration::hours(i as i64);
        out.push(ts.format("%Y-%m-%dT%H:00:00Z").to_string());
    }
    out
}

/// Compute (numeric_score, letter_grade) from the event window. Returns
/// `(None, None)` when no `rule_scan` session anchors the window — a quiet
/// instance should show "unknown", not a fake A.
fn compute_grade(events: &[Event]) -> (Option<f64>, Option<Value>) {
    if events.iter().all(|e| e.hook != "rule_scan") {
        return (None, None);
    }
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
    let letter = serde_json::to_value(report.grade).ok();
    (Some(report.score), letter)
}

/// Percentage of `rule_check` events in the window that blocked. Returns `0.0`
/// when no rule_check events fired.
///
/// The denominator is every rule_check (Pass/Warn/Block), NOT just blocks —
/// `harness_observe::stats::linter_feedback_count` already filters to blocks,
/// so using it as the total would collapse the metric to 0% / 100%.
fn compute_rule_fail_rate_pct(events: &[Event]) -> f64 {
    let (total, failed) =
        events
            .iter()
            .filter(|e| e.hook == "rule_check")
            .fold((0u64, 0u64), |(t, f), e| {
                let blocked = u64::from(matches!(e.decision, Decision::Block));
                (t + 1, f + blocked)
            });
    if total == 0 {
        return 0.0;
    }
    (failed as f64 / total as f64) * 100.0
}

/// Build the cross-project live activity feed from the latest events.
fn build_feed(events: &[Event], now: DateTime<Utc>) -> Vec<Value> {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.ts));
    sorted
        .into_iter()
        .take(FEED_LIMIT)
        .map(|e| {
            let level = match e.decision {
                Decision::Block | Decision::Escalate => "err",
                Decision::Warn => "warn",
                Decision::Pass | Decision::Complete => "ok",
                Decision::Gate => "",
            };
            let body = e
                .reason
                .clone()
                .or_else(|| e.detail.clone())
                .unwrap_or_default();
            json!({
                "ts": e.ts.to_rfc3339(),
                "ago": relative_ago(now, e.ts),
                "kind": e.hook,
                "tool": e.tool,
                "body": body,
                "level": level,
                "project": "",
            })
        })
        .collect()
}

/// Derive open-alert rows from the event window and current runtime state.
fn build_alerts(events: &[Event], hosts: &[crate::runtime_hosts::RuntimeHostInfo]) -> Vec<Value> {
    let mut out = Vec::new();

    let offline: Vec<&crate::runtime_hosts::RuntimeHostInfo> =
        hosts.iter().filter(|h| !h.online).collect();
    if !offline.is_empty() {
        let names: Vec<&str> = offline.iter().map(|h| h.display_name.as_str()).collect();
        out.push(json!({
            "level": "warn",
            "msg": format!("{} runtime(s) offline", offline.len()),
            "sub": names.join(" · "),
            "ts": Value::Null,
        }));
    }

    let recent_blocks = events
        .iter()
        .filter(|e| e.hook == "rule_check" && matches!(e.decision, Decision::Block))
        .count();
    if recent_blocks > 0 {
        out.push(json!({
            "level": "err",
            "msg": format!("{} rule violation(s) in last {}h", recent_blocks, OVERVIEW_WINDOW_HOURS),
            "sub": "see observability page",
            "ts": Value::Null,
        }));
    }

    out
}

/// Human-readable delta such as `14s`, `3m`, `2h`, `1d` — matches the feed
/// column spacing in the design.
fn relative_ago(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use axum::{body::to_bytes, routing::get, Router};

    #[test]
    fn hour_axis_has_expected_length_and_order() {
        let now = Utc::now();
        let axis = build_hour_axis(now);
        assert_eq!(axis.len(), THROUGHPUT_BUCKETS);
        // Oldest first, strictly increasing.
        for pair in axis.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn relative_ago_formats_each_bucket() {
        let now = Utc::now();
        assert!(relative_ago(now, now).ends_with('s'));
        assert!(relative_ago(now, now - Duration::minutes(5)).ends_with('m'));
        assert!(relative_ago(now, now - Duration::hours(3)).ends_with('h'));
        assert!(relative_ago(now, now - Duration::days(2)).ends_with('d'));
    }

    #[test]
    fn rule_fail_rate_zero_when_no_events() {
        assert_eq!(compute_rule_fail_rate_pct(&[]), 0.0);
    }

    #[tokio::test]
    async fn overview_returns_expected_shape() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        if !test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = test_helpers::tempdir_in_home("harness-test-overview-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/overview", get(overview))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/overview")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        for key in [
            "window",
            "kpi",
            "distribution",
            "throughput",
            "projects",
            "runtimes",
            "heatmap",
            "feed",
            "alerts",
            "global",
        ] {
            assert!(body.get(key).is_some(), "missing top-level key: {key}");
        }
        assert!(body["kpi"]["active_tasks"].is_number());
        assert!(body["kpi"]["merged_24h"].is_number());
        assert!(body["kpi"]["worktrees"]["used"].is_number());
        assert!(body["kpi"]["worktrees"]["total"].is_number());
        assert!(body["throughput"]["hours"].is_array());
        assert_eq!(
            body["throughput"]["hours"].as_array().unwrap().len(),
            THROUGHPUT_BUCKETS
        );

        Ok(())
    }

    #[tokio::test]
    async fn worktrees_used_includes_local_workspace_manager() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        if !test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let dir = test_helpers::tempdir_in_home("harness-test-overview-wt-")?;
        let mut state = test_helpers::make_test_state(dir.path()).await?;

        // Seed the workspace manager with 3 live entries (no real git ops needed).
        let ws_config = harness_core::config::misc::WorkspaceConfig {
            root: dir.path().join("ws"),
            ..Default::default()
        };
        let mgr = Arc::new(crate::workspace::WorkspaceManager::new(ws_config)?);
        for i in 0..3u64 {
            mgr.active.insert(
                harness_core::types::TaskId(format!("fake-{i}")),
                crate::workspace::ActiveWorkspace {
                    workspace_path: dir.path().join(format!("ws/fake-{i}")),
                    source_repo: dir.path().to_path_buf(),
                    owner_session: format!("session-{i}"),
                    run_generation: 1,
                },
            );
        }
        state.concurrency.workspace_mgr = Some(mgr);

        let app = Router::new()
            .route("/api/overview", get(overview))
            .with_state(Arc::new(state));

        let req = axum::http::Request::builder()
            .uri("/api/overview")
            .body(axum::body::Body::empty())?;

        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert_eq!(
            body["kpi"]["worktrees"]["used"].as_u64(),
            Some(3),
            "kpi.worktrees.used should count local workspace manager entries"
        );

        Ok(())
    }
}
