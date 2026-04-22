//! GET /api/operator-snapshot — live operator diagnostic view.
//!
//! Aggregates retry scheduler state, rate-limit pressure, and recent task
//! failures into a single low-latency payload suitable for polling every 30 s.

use crate::http::AppState;
use axum::{extract::State, http::StatusCode, Json};
use chrono::Utc;
use harness_core::types::EventFilters;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Tasks stalled for longer than this are included in the snapshot.
/// Approximation for operator visibility only — does not affect retry decisions.
const SNAPSHOT_STALE_MINS: u64 = 30;

/// Maximum stalled / recent-failure tasks returned to keep the payload bounded.
const MAX_TASKS: usize = 20;

/// Maximum length of a task error string before truncation.
const MAX_ERROR_LEN: usize = 200;

pub async fn operator_snapshot(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let generated_at = Utc::now();

    // Bound the retry-event query to the last 2 hours so the endpoint does not
    // scan unbounded history on long-lived instances.  We only need `.last()`,
    // so any window that comfortably covers one retry interval is sufficient.
    let retry_filter = EventFilters {
        hook: Some("periodic_retry:summary".to_string()),
        since: Some(generated_at - chrono::Duration::hours(2)),
        ..Default::default()
    };
    // Collect all subsections concurrently — they are independent.
    let (retry_events_res, stalled_res, failed_res) = tokio::join!(
        state.observability.events.query(&retry_filter),
        state
            .core
            .tasks
            .list_stalled_tasks(Duration::from_secs(SNAPSHOT_STALE_MINS * 60), None),
        state.core.tasks.list_recent_failed(MAX_TASKS as i64),
    );

    // ---- retry section ----
    let retry_events = retry_events_res.unwrap_or_else(|e| {
        tracing::warn!("operator_snapshot: failed to query retry events: {e}");
        Vec::new()
    });
    // query returns ASC order; the last element is the most recent tick.
    let last_tick: Value = retry_events
        .last()
        .and_then(|ev| {
            let detail = ev.detail.as_deref()?;
            let parsed: serde_json::Value = serde_json::from_str(detail).ok()?;
            Some(json!({
                "checked": parsed["checked"],
                "retried": parsed["retried"],
                "stuck":   parsed["stuck"],
                "skipped": parsed["skipped"],
                "at":      ev.ts.to_rfc3339(),
            }))
        })
        .unwrap_or(Value::Null);

    let stalled_tasks = stalled_res.unwrap_or_else(|e| {
        tracing::warn!("operator_snapshot: failed to query stalled tasks: {e}");
        Vec::new()
    });
    let stalled_json: Vec<Value> = stalled_tasks
        .iter()
        .take(MAX_TASKS)
        .map(|t| {
            json!({
                "task_id":      t.id.0,
                "external_id":  t.external_id.as_deref().unwrap_or("—"),
                "project":      t.project_root.as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "—".to_string()),
                "status":       t.status.as_ref(),
                "stalled_since": t.updated_at.as_deref().unwrap_or("—"),
            })
        })
        .collect();

    // ---- rate-limit section ----
    let sig_snap = state.observability.signal_rate_limiter.snapshot();
    let pw_snap = state.observability.password_reset_rate_limiter.snapshot();

    // ---- recent failures section ----
    let failed_tasks = failed_res.unwrap_or_else(|e| {
        tracing::warn!("operator_snapshot: failed to query recent failures: {e}");
        Vec::new()
    });
    let failures_json: Vec<Value> = failed_tasks
        .iter()
        .map(|t| {
            let error = t
                .error
                .as_deref()
                .map(|e| {
                    if e.len() > MAX_ERROR_LEN {
                        // Walk back to a valid char boundary to avoid panicking
                        // on multi-byte UTF-8 characters at the cut point.
                        let mut boundary = MAX_ERROR_LEN;
                        while boundary > 0 && !e.is_char_boundary(boundary) {
                            boundary -= 1;
                        }
                        format!("{}…", &e[..boundary])
                    } else {
                        e.to_string()
                    }
                })
                .unwrap_or_else(|| "—".to_string());
            json!({
                "task_id":    t.id.0,
                "external_id": t.external_id.as_deref().unwrap_or("—"),
                "project":    t.project.as_deref().unwrap_or("—"),
                "error":      error,
                "failed_at":  t.failed_at.as_deref().unwrap_or("—"),
            })
        })
        .collect();

    let body = json!({
        "generated_at": generated_at.to_rfc3339(),
        "retry": {
            "last_tick":     last_tick,
            "stalled_tasks": stalled_json,
        },
        "rate_limits": {
            "signal_ingestion": {
                "tracked_sources": sig_snap.tracked_sources,
                "limit_per_minute": sig_snap.limit_per_minute,
            },
            "password_reset": {
                "tracked_identifiers": pw_snap.tracked_identifiers,
                "limit_per_hour": pw_snap.limit_per_hour,
            },
        },
        "recent_failures": failures_json,
    });

    (StatusCode::OK, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use axum::{body::to_bytes, routing::get, Router};

    #[tokio::test]
    async fn returns_200_with_all_top_level_keys() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        for key in ["generated_at", "retry", "rate_limits", "recent_failures"] {
            assert!(body.get(key).is_some(), "missing top-level key: {key}");
        }
        assert!(body["retry"].get("last_tick").is_some());
        assert!(body["retry"].get("stalled_tasks").is_some());
        assert!(body["rate_limits"].get("signal_ingestion").is_some());
        assert!(body["rate_limits"].get("password_reset").is_some());
        Ok(())
    }

    #[tokio::test]
    async fn last_tick_null_on_fresh_server() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-notick-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert!(
            body["retry"]["last_tick"].is_null(),
            "expected null last_tick on fresh server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stalled_tasks_empty_on_fresh_server() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-nostall-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert_eq!(
            body["retry"]["stalled_tasks"].as_array().map(|a| a.len()),
            Some(0),
        );
        Ok(())
    }

    #[tokio::test]
    async fn recent_failures_empty_on_fresh_server() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-nofail-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        assert_eq!(body["recent_failures"].as_array().map(|a| a.len()), Some(0),);
        Ok(())
    }

    #[tokio::test]
    async fn recent_failures_capped_at_max() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-cap-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        // Seed 25 failed tasks.
        for i in 0..25u32 {
            let mut task = crate::task_runner::TaskState {
                id: harness_core::types::TaskId(format!("fail-task-{i}")),
                status: crate::task_runner::TaskStatus::Failed,
                turn: 1,
                pr_url: None,
                rounds: vec![],
                error: Some(format!("error {i}")),
                source: None,
                external_id: Some(format!("issue:{i}")),
                parent_id: None,
                depends_on: vec![],
                subtask_ids: vec![],
                project_root: Some(std::path::PathBuf::from("/test/proj")),
                issue: None,
                repo: None,
                description: None,
                created_at: None,
                updated_at: None,
                priority: 0,
                phase: crate::task_runner::TaskPhase::Implement,
                triage_output: None,
                plan_output: None,
                request_settings: None,
            };
            task.status = crate::task_runner::TaskStatus::Failed;
            state.core.tasks.insert(&task).await;
        }

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        let failures = body["recent_failures"].as_array().expect("array");
        assert!(
            failures.len() <= MAX_TASKS,
            "recent_failures should be capped at {MAX_TASKS}, got {}",
            failures.len()
        );
        Ok(())
    }

    #[tokio::test]
    async fn long_error_is_truncated() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-trunc-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let long_error = "x".repeat(500);
        let task = crate::task_runner::TaskState {
            id: harness_core::types::TaskId("trunc-task".to_string()),
            status: crate::task_runner::TaskStatus::Failed,
            turn: 1,
            pr_url: None,
            rounds: vec![],
            error: Some(long_error),
            source: None,
            external_id: Some("issue:trunc".to_string()),
            parent_id: None,
            depends_on: vec![],
            subtask_ids: vec![],
            project_root: Some(std::path::PathBuf::from("/test/proj")),
            issue: None,
            repo: None,
            description: None,
            created_at: None,
            updated_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
        };
        state.core.tasks.insert(&task).await;

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;

        let failures = body["recent_failures"].as_array().expect("array");
        assert!(!failures.is_empty());
        let error_str = failures[0]["error"].as_str().expect("string");
        assert!(
            error_str.len() <= MAX_ERROR_LEN + 4, // +4 for the "…" suffix (multibyte)
            "error should be truncated, got len {}",
            error_str.len()
        );
        Ok(())
    }

    /// Regression test: multi-byte UTF-8 at the truncation boundary must not panic.
    /// A 3-byte emoji repeated so that the cut falls mid-character verifies the
    /// is_char_boundary walk-back in the truncation logic.
    #[tokio::test]
    async fn unicode_error_truncation_does_not_panic() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-op-snap-unicode-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        // "€" is 3 bytes; repeat enough times that the 200-byte cut falls mid-char.
        let unicode_error = "€".repeat(100); // 300 bytes total
        let task = crate::task_runner::TaskState {
            id: harness_core::types::TaskId("unicode-task".to_string()),
            status: crate::task_runner::TaskStatus::Failed,
            turn: 1,
            pr_url: None,
            rounds: vec![],
            error: Some(unicode_error),
            source: None,
            external_id: Some("issue:unicode".to_string()),
            parent_id: None,
            depends_on: vec![],
            subtask_ids: vec![],
            project_root: Some(std::path::PathBuf::from("/test/proj")),
            issue: None,
            repo: None,
            description: None,
            created_at: None,
            updated_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
        };
        state.core.tasks.insert(&task).await;

        let app = Router::new()
            .route("/api/operator-snapshot", get(operator_snapshot))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-snapshot")
            .body(axum::body::Body::empty())?;
        // Must not panic (no 500).
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;
        let failures = body["recent_failures"].as_array().expect("array");
        assert!(!failures.is_empty());
        let error_str = failures[0]["error"].as_str().expect("string");
        // Result must be valid UTF-8 (serde_json already guarantees this) and bounded.
        assert!(error_str.len() <= MAX_ERROR_LEN + 4);
        Ok(())
    }
}
