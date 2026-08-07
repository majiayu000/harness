use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Health registry for background orchestration loops (GH-1880).
///
/// Every `tokio::spawn` loop registers a `LoopHandle` and reports tick
/// outcomes; operators see per-loop liveness (last tick, last success,
/// failures) plus a single aggregated config-parse failure so a malformed
/// WORKFLOW.md cannot silently disable retention, watchdog, and reaping
/// while dispatch keeps spending.
#[derive(Debug, Default)]
pub(crate) struct BackgroundLoopHealth {
    loops: Mutex<HashMap<&'static str, LoopStatus>>,
    config_failure: Mutex<Option<ConfigFailure>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct LoopStatus {
    tick_count: u64,
    failure_count: u64,
    last_tick_at: Option<Instant>,
    last_success_at: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LoopSnapshot {
    pub(crate) name: &'static str,
    pub(crate) tick_count: u64,
    pub(crate) failure_count: u64,
    pub(crate) last_tick_secs_ago: Option<u64>,
    pub(crate) last_success_secs_ago: Option<u64>,
    pub(crate) last_error: Option<String>,
    pub(crate) stale: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ConfigFailureSnapshot {
    pub(crate) first_seen_at: DateTime<Utc>,
    pub(crate) last_seen_at: DateTime<Utc>,
    pub(crate) occurrences: u64,
    pub(crate) affected_loops: Vec<&'static str>,
    pub(crate) last_error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigFailure {
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    occurrences: u64,
    affected_loops: Vec<&'static str>,
    last_error: String,
}

impl BackgroundLoopHealth {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register (or re-register) a loop and return its handle.
    pub(crate) fn register_loop(self: &Arc<Self>, name: &'static str) -> LoopHandle {
        self.loops
            .lock()
            .expect("loop health mutex poisoned")
            .entry(name)
            .or_default();
        LoopHandle {
            health: Arc::clone(self),
            name,
        }
    }

    /// Record that `name` completed a tick successfully.
    pub(crate) fn record_tick(&self, name: &'static str) {
        if let Some(status) = self
            .loops
            .lock()
            .expect("loop health mutex poisoned")
            .get_mut(name)
        {
            status.tick_count = status.tick_count.saturating_add(1);
            status.last_tick_at = Some(Instant::now());
            status.last_success_at = Some(Instant::now());
        }
    }

    /// Record that `name` failed a tick (warn-and-continue loops report this
    /// so the failure is visible even though the loop keeps running).
    ///
    /// Only successful ticks advance `tick_count`; a failed tick advances
    /// `failure_count` and keeps the last error, so operators see both the
    /// success/failure split and the reason.
    pub(crate) fn record_tick_failure(&self, name: &'static str, error: &str) {
        let mut loops = self.loops.lock().expect("loop health mutex poisoned");
        if let Some(status) = loops.get_mut(name) {
            status.failure_count = status.failure_count.saturating_add(1);
            status.last_tick_at = Some(Instant::now());
            status.last_error = Some(error.to_string());
        }
    }

    /// Record a workflow-config parse failure affecting `name`. Aggregated so
    /// every consumer reports into one operator-visible signal.
    pub(crate) fn record_config_failure(&self, name: &'static str, error: &str) {
        let now = Utc::now();
        let mut guard = self
            .config_failure
            .lock()
            .expect("loop health mutex poisoned");
        match guard.as_mut() {
            Some(failure) => {
                failure.last_seen_at = now;
                failure.occurrences = failure.occurrences.saturating_add(1);
                failure.last_error = error.to_string();
                if !failure.affected_loops.contains(&name) {
                    failure.affected_loops.push(name);
                }
            }
            None => {
                guard.replace(ConfigFailure {
                    first_seen_at: now,
                    last_seen_at: now,
                    occurrences: 1,
                    affected_loops: vec![name],
                    last_error: error.to_string(),
                });
            }
        }
    }

    /// Clear the aggregated config failure (e.g. after a successful parse).
    pub(crate) fn clear_config_failure(&self) {
        self.config_failure
            .lock()
            .expect("loop health mutex poisoned")
            .take();
    }

    pub(crate) fn snapshot(&self, max_stale_secs: u64) -> Vec<LoopSnapshot> {
        let now = Instant::now();
        self.loops
            .lock()
            .expect("loop health mutex poisoned")
            .iter()
            .map(|(name, status)| LoopSnapshot {
                name,
                tick_count: status.tick_count,
                failure_count: status.failure_count,
                last_tick_secs_ago: status.last_tick_at.map(|t| now.duration_since(t).as_secs()),
                last_success_secs_ago: status
                    .last_success_at
                    .map(|t| now.duration_since(t).as_secs()),
                last_error: status.last_error.clone(),
                stale: status
                    .last_tick_at
                    .map(|t| now.duration_since(t).as_secs() > max_stale_secs)
                    .unwrap_or(true),
            })
            .collect()
    }

    pub(crate) fn config_failure_snapshot(&self) -> Option<ConfigFailureSnapshot> {
        self.config_failure
            .lock()
            .expect("loop health mutex poisoned")
            .as_ref()
            .map(|failure| ConfigFailureSnapshot {
                first_seen_at: failure.first_seen_at,
                last_seen_at: failure.last_seen_at,
                occurrences: failure.occurrences,
                affected_loops: failure.affected_loops.clone(),
                last_error: failure.last_error.clone(),
            })
    }
}

/// Per-loop reporting handle.
#[derive(Debug, Clone)]
pub(crate) struct LoopHandle {
    health: Arc<BackgroundLoopHealth>,
    name: &'static str,
}

impl LoopHandle {
    pub(crate) fn tick_ok(&self) {
        self.health.record_tick(self.name);
    }

    pub(crate) fn tick_failed(&self, error: &str) {
        self.health.record_tick_failure(self.name, error);
    }

    pub(crate) fn config_failure(&self, error: &str) {
        self.health.record_config_failure(self.name, error);
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn snapshot_tracks_ticks_and_failures() {
        let health = Arc::new(BackgroundLoopHealth::new());
        let handle = health.register_loop("test-loop");
        assert!(
            health.snapshot(60)[0].stale,
            "unregistered loop starts stale"
        );
        handle.tick_ok();
        handle.tick_ok();
        handle.tick_failed("boom");
        let snapshot = health.snapshot(60);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[0].tick_count, 2,
            "failed ticks do not count as successes"
        );
        assert_eq!(snapshot[0].failure_count, 1);
        assert_eq!(snapshot[0].last_error.as_deref(), Some("boom"));
        assert!(!snapshot[0].stale);
    }

    #[test]
    fn config_failure_is_aggregated_and_cleared() {
        let health = Arc::new(BackgroundLoopHealth::new());
        let watchdog = health.register_loop("workflow_watchdog");
        let retention = health.register_loop("runtime_retention");
        watchdog.config_failure("parse error one");
        retention.config_failure("parse error one");

        let failure = health.config_failure_snapshot().expect("failure recorded");
        assert_eq!(failure.occurrences, 2);
        assert_eq!(
            failure.affected_loops,
            vec!["workflow_watchdog", "runtime_retention"]
        );
        assert_eq!(failure.last_error, "parse error one");

        health.clear_config_failure();
        assert!(health.config_failure_snapshot().is_none());
    }

    #[test]
    fn stale_detection_uses_last_tick() {
        let health = Arc::new(BackgroundLoopHealth::new());
        let handle = health.register_loop("stale-loop");
        // A loop that never ticked is stale at any threshold.
        assert!(health.snapshot(60)[0].stale);
        handle.tick_ok();
        assert!(!health.snapshot(60)[0].stale);
        std::thread::sleep(Duration::from_millis(5));
        // Second granularity truncation: a 5ms-old tick is not stale at any
        // whole-second threshold >= 1.
        assert!(!health.snapshot(1)[0].stale);
    }
}
