//! GET /api/overview — system-level aggregated view across all projects.
//!
//! Feeds the `/overview` HTML page (served from `crate::overview::index`).
//! Aggregates data from existing services rather than introducing new
//! tracking: task counts come from `TaskService`, runtime fleet state from
//! `RuntimeHostManager`, quality grade and rule-fail rate from the event
//! store. Metrics that harness does not yet track (runtime CPU/RAM) are
//! returned as `null` so the UI degrades gracefully.

use crate::http::AppState;
use crate::runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection};
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Duration, Timelike, Utc};
use harness_core::types::{Decision, Event, EventFilters};
use harness_observe::quality::QualityGrader;
use harness_observe::usage::UsageMetrics;
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
    let active_counts = active_task_overview_counts(&state).await;
    let running = active_counts.running;
    let queued = active_counts.queued;
    let max_concurrent = tq.global_limit();

    let dashboard_counts = state.task_svc.count_for_dashboard().await;
    let global_done = dashboard_counts.global_done;
    let global_failed = dashboard_counts.global_failed;
    let global_stalled = dashboard_counts.global_stalled;

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

    let usage_events = match state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("llm_usage".to_string()),
            since: Some(since_dt),
            include_content: true,
            ..Default::default()
        })
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("overview: failed to query llm_usage events: {e}");
            Vec::new()
        }
    };
    let usage_summary = aggregate_llm_usage(&usage_events);

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
        let active_project_counts = active_counts
            .by_project
            .get(&key)
            .copied()
            .unwrap_or_default();
        let counts = dashboard_counts.by_project.get(&key);
        let done = counts.map_or(0, |c| c.done);
        let failed = counts.map_or(0, |c| c.failed);
        let stalled = counts.map_or(0, |c| c.stalled);
        let buckets = per_project_buckets
            .remove(&key)
            .unwrap_or_else(|| vec![0; THROUGHPUT_BUCKETS]);
        let merged_window: u64 = buckets.iter().sum();
        projects_json.push(json!({
            "id": p.id,
            "root": p.root,
            "running": active_project_counts.running,
            "queued": active_project_counts.queued,
            "done": done,
            "failed": failed,
            "stalled": stalled,
            "merged_24h": merged_window,
            "trend": buckets,
            "avg_score": Value::Null,
            "worktrees": Value::Null,
            "tokens_24h": usage_summary.project_tokens_json(&key),
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
        let active_leases =
            match super::runtime_hosts::active_runtime_job_lease_count(&state, &host.id).await {
                Ok(count) => Some(count),
                Err(error) => {
                    tracing::warn!(
                        host_id = %host.id,
                        "overview: failed to count runtime-job leases: {error}"
                    );
                    None
                }
            };
        if let Some(active_leases) = active_leases {
            worktrees_used = worktrees_used.saturating_add(active_leases);
        }
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
    // Exhausted outbound alert deliveries in the window (GH1582 B-008).
    let alert_delivery_failures = state
        .observability
        .events
        .query_external_signals(Some(now - chrono::Duration::hours(OVERVIEW_WINDOW_HOURS)))
        .map(|signals| {
            signals
                .iter()
                .filter(|s| s.source == "alerting" && s.payload["outcome"] == "exhausted")
                .count()
        })
        .unwrap_or(0);
    let alerts = build_alerts(&events, &runtime_hosts, alert_delivery_failures);

    let evolution: Value = events
        .iter()
        .filter(|e| e.hook == "self_evolution_tick")
        .max_by_key(|e| e.ts)
        .and_then(|e| parse_evolution_from_reason(e.reason.as_deref()))
        .unwrap_or(Value::Null);

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
            "active_tasks": active_counts.total,
            "merged_24h": merged_24h,
            "avg_review_score": avg_review_score,
            "grade": grade_letter,
            "rule_fail_rate_pct": rule_fail_rate_pct,
            "tokens_24h": usage_summary.total_tokens_json(),
            "worktrees": {
                "used": worktrees_used,
                "total": max_concurrent,
            },
        },
        "agent_tokens": usage_summary.agent_tokens_json(),
        "distribution": {
            "queued": queued,
            "running": running,
            "review": 0,
            "merged": global_done,
            "failed": global_failed,
            "stalled": global_stalled,
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
        "evolution": evolution,
        "global": {
            "uptime_secs": uptime_secs,
            "running": running,
            "queued": queued,
            "done": global_done,
            "failed": global_failed,
            "stalled": global_stalled,
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

#[derive(Debug, Default)]
struct LlmUsageSummary {
    has_events: bool,
    total_tokens: u64,
    by_project: HashMap<String, u64>,
    by_agent: HashMap<String, u64>,
}

impl LlmUsageSummary {
    fn add(&mut self, record: LlmUsageRecord) {
        self.has_events = true;
        self.total_tokens = self.total_tokens.saturating_add(record.tokens);
        if !record.project.is_empty() {
            let entry = self.by_project.entry(record.project).or_default();
            *entry = entry.saturating_add(record.tokens);
        }
        let entry = self.by_agent.entry(record.agent).or_default();
        *entry = entry.saturating_add(record.tokens);
    }

    fn total_tokens_json(&self) -> Value {
        if self.has_events {
            json!(self.total_tokens)
        } else {
            Value::Null
        }
    }

    fn project_tokens_json(&self, project: &str) -> Value {
        self.by_project
            .get(project)
            .copied()
            .map_or(Value::Null, |tokens| json!(tokens))
    }

    fn agent_tokens_json(&self) -> Value {
        let mut agents: Vec<_> = self.by_agent.iter().collect();
        agents.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        Value::Array(
            agents
                .into_iter()
                .map(|(agent, tokens)| {
                    json!({
                        "agent": agent,
                        "tokens_24h": tokens,
                    })
                })
                .collect(),
        )
    }
}

#[derive(Debug)]
struct LlmUsageRecord {
    agent: String,
    project: String,
    tokens: u64,
}

fn aggregate_llm_usage(events: &[Event]) -> LlmUsageSummary {
    let mut summary = LlmUsageSummary::default();
    for event in events {
        match parse_llm_usage_event(event) {
            Some(record) => summary.add(record),
            None => {
                tracing::warn!(event_id = %event.id.as_str(), "overview: skipped malformed llm_usage event")
            }
        }
    }
    summary
}

fn parse_llm_usage_event(event: &Event) -> Option<LlmUsageRecord> {
    if event.hook != "llm_usage" {
        return None;
    }
    let content = event.content.as_deref()?;
    let payload: Value = serde_json::from_str(content).ok()?;
    let usage = UsageMetrics::from_payload(&payload)?;
    let tokens = usage.total_tokens();
    let agent = payload
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(event.tool.as_str())
        .to_string();
    let project = payload
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Some(LlmUsageRecord {
        agent,
        project,
        tokens,
    })
}

#[derive(Debug, Default)]
pub(crate) struct ActiveTaskOverviewCounts {
    pub(crate) total: u64,
    pub(crate) running: u64,
    pub(crate) queued: u64,
    pub(crate) by_project: HashMap<String, ActiveProjectCounts>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ActiveProjectCounts {
    pub(crate) running: u64,
    pub(crate) queued: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTaskBucket {
    Running,
    Queued,
}

impl ActiveTaskOverviewCounts {
    fn add(&mut self, project: Option<&str>, bucket: ActiveTaskBucket) {
        self.total += 1;
        let project_counts = project.map(|project| self.by_project.entry(project.to_string()));
        match bucket {
            ActiveTaskBucket::Running => {
                self.running += 1;
                if let Some(entry) = project_counts {
                    entry.or_default().running += 1;
                }
            }
            ActiveTaskBucket::Queued => {
                self.queued += 1;
                if let Some(entry) = project_counts {
                    entry.or_default().queued += 1;
                }
            }
        }
    }
}

pub(crate) async fn active_task_overview_counts(state: &AppState) -> ActiveTaskOverviewCounts {
    let mut counts = ActiveTaskOverviewCounts::default();
    let mut counted_runtime_active_workflows = false;

    if let Some(store) = state.core.workflow_runtime_store.as_ref() {
        match crate::handlers::definition_ids::active_count_definition_ids() {
            Ok(definition_ids) => {
                for definition_id in &definition_ids {
                    match store
                        .list_nonterminal_instances_by_definition(definition_id, None, None)
                        .await
                    {
                        Ok(workflows) => {
                            for workflow in workflows {
                                counted_runtime_active_workflows |=
                                    add_active_runtime_workflow(&mut counts, &workflow);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                definition_id = definition_id.as_str(),
                                "overview: failed to list runtime workflows for active counts: {error}"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!("overview: {error}; runtime workflow active counts unavailable");
            }
        }
    }

    if !counted_runtime_active_workflows {
        for _ in 0..state.concurrency.task_queue.running_count() {
            counts.add(None, ActiveTaskBucket::Running);
        }
        for _ in 0..state.concurrency.task_queue.queued_count() {
            counts.add(None, ActiveTaskBucket::Queued);
        }
    }

    counts
}

fn add_active_runtime_workflow(
    counts: &mut ActiveTaskOverviewCounts,
    workflow: &harness_workflow::runtime::WorkflowInstance,
) -> bool {
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
    let Some(bucket) = projection.active_bucket() else {
        return false;
    };
    let bucket = match bucket {
        RuntimeActiveBucket::Running => ActiveTaskBucket::Running,
        RuntimeActiveBucket::Queued => ActiveTaskBucket::Queued,
    };
    counts.add(projection.project_id.as_deref(), bucket);
    true
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
        .filter(|e| e.hook != "llm_usage")
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
fn build_alerts(
    events: &[Event],
    hosts: &[crate::runtime_hosts::RuntimeHostInfo],
    alert_delivery_failures: usize,
) -> Vec<Value> {
    let mut out = Vec::new();

    if alert_delivery_failures > 0 {
        out.push(json!({
            "level": "err",
            "msg": format!(
                "{alert_delivery_failures} outbound alert delivery failure(s) in last {OVERVIEW_WINDOW_HOURS}h"
            ),
            "sub": "see alerting audit trail (signals)",
            "ts": Value::Null,
        }));
    }

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

/// Parse `drafts_pending`, `drafts_adopted`, and `skills_invoked` from the
/// space-separated `key=value` reason string of a `self_evolution_tick` event.
///
/// Returns `None` when any of the three required fields are absent — this
/// gracefully degrades old events (pre-#972) to `null` on the dashboard.
fn parse_evolution_from_reason(reason: Option<&str>) -> Option<Value> {
    let reason = reason?;
    let mut map = std::collections::HashMap::new();
    for part in reason.split_whitespace() {
        if let Some((k, v)) = part.split_once('=') {
            map.insert(k, v);
        }
    }
    let drafts_pending = map.get("drafts_pending")?.parse::<u64>().ok()?;
    let drafts_auto_adopted = map.get("drafts_adopted")?.parse::<u64>().ok()?;
    let skills_invoked_in_window = map.get("skills_invoked")?.parse::<u64>().ok()?;
    Some(json!({
        "drafts_pending": drafts_pending,
        "drafts_auto_adopted": drafts_auto_adopted,
        "skills_invoked_in_window": skills_invoked_in_window,
    }))
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
#[path = "overview_tests.rs"]
mod overview_tests;
