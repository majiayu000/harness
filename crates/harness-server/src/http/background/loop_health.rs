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

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopStatus {
    tick_count: u64,
    failure_count: u64,
    last_tick_at: Option<Instant>,
    last_success_at: Option<Instant>,
    last_error: Option<String>,
    /// Configured cadence of the loop in seconds (0 when unknown). Slow loops
    /// declare this so staleness is judged against their own interval instead
    /// of the global floor; a 24h scheduler tick is healthy at hour 12.
    expected_interval_secs: u64,
    /// When the loop registered. A loop that has not produced its first tick
    /// yet is judged from this instant instead of being treated as stale.
    registered_at: Instant,
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
        self.register_loop_with_interval(name, 0)
    }

    /// Register (or re-register) a loop with its configured tick cadence, in
    /// seconds. Loops slower than the global staleness floor must declare
    /// their interval or the operator monitor would report them stale between
    /// every normal tick (GH-1981).
    pub(crate) fn register_loop_with_interval(
        self: &Arc<Self>,
        name: &'static str,
        expected_interval_secs: u64,
    ) -> LoopHandle {
        let mut loops = self.loops.lock().expect("loop health mutex poisoned");
        match loops.entry(name) {
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                // Re-registration refreshes only the cadence hint; liveness
                // history belongs to the running server, not the spawn site.
                occupied.get_mut().expected_interval_secs = expected_interval_secs;
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(LoopStatus {
                    tick_count: 0,
                    failure_count: 0,
                    last_tick_at: None,
                    last_success_at: None,
                    last_error: None,
                    expected_interval_secs,
                    registered_at: Instant::now(),
                });
            }
        }
        LoopHandle {
            health: Arc::clone(self),
            name,
        }
    }

    /// Update the expected tick cadence of an already-registered loop. Called
    /// by loops whose interval is reloaded from config on every iteration.
    pub(crate) fn set_loop_interval(&self, name: &'static str, expected_interval_secs: u64) {
        if let Some(status) = self
            .loops
            .lock()
            .expect("loop health mutex poisoned")
            .get_mut(name)
        {
            status.expected_interval_secs = expected_interval_secs;
        }
    }

    /// Staleness threshold for one loop: the global floor, raised to cover the
    /// loop's own cadence with 50% slack. A healthy slow loop never crosses
    /// it; a dead one does after at most one-and-a-half missed intervals.
    fn stale_threshold_secs(max_stale_secs: u64, expected_interval_secs: u64) -> u64 {
        max_stale_secs.max(expected_interval_secs.saturating_add(expected_interval_secs / 2))
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
                last_tick_secs_ago: status
                    .last_tick_at
                    .map(|t| now.saturating_duration_since(t).as_secs()),
                last_success_secs_ago: status
                    .last_success_at
                    .map(|t| now.saturating_duration_since(t).as_secs()),
                last_error: status.last_error.clone(),
                stale: {
                    let threshold =
                        Self::stale_threshold_secs(max_stale_secs, status.expected_interval_secs);
                    // A loop that has not ticked yet is judged from when it
                    // registered, not treated as instantly stale: sleep-first
                    // loops (daily schedulers, delayed initial scans) are
                    // healthy between spawn and their first real tick.
                    let since = status.last_tick_at.unwrap_or(status.registered_at);
                    now.saturating_duration_since(since).as_secs() > threshold
                },
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

    /// Update the expected tick cadence of this loop (config-driven loops
    /// call this after each config reload).
    pub(crate) fn set_interval(&self, expected_interval_secs: u64) {
        self.health
            .set_loop_interval(self.name, expected_interval_secs);
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
            !health.snapshot(60)[0].stale,
            "a loop that has not ticked yet is fresh until its threshold passes"
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
        // A loop that has not ticked yet is judged from its registration
        // time, so a fresh registration is healthy at the global floor.
        assert!(!health.snapshot(60)[0].stale);
        handle.tick_ok();
        assert!(!health.snapshot(60)[0].stale);
        std::thread::sleep(Duration::from_millis(5));
        // Second granularity truncation: a 5ms-old tick is not stale at any
        // whole-second threshold >= 1.
        assert!(!health.snapshot(1)[0].stale);
    }

    #[test]
    fn never_ticked_loop_expires_after_registration_window() {
        let health = Arc::new(BackgroundLoopHealth::new());
        health
            .loops
            .lock()
            .expect("loop health mutex poisoned")
            .insert(
                "ghost-loop",
                LoopStatus {
                    tick_count: 0,
                    failure_count: 0,
                    last_tick_at: None,
                    last_success_at: None,
                    last_error: None,
                    expected_interval_secs: 0,
                    registered_at: Instant::now()
                        .checked_sub(Duration::from_secs(120))
                        .expect("clock moved backwards"),
                },
            );
        // Registered 120s ago with no tick and no interval hint: past the
        // 60s floor, the loop is genuinely stale.
        assert!(health.snapshot(60)[0].stale);
    }

    #[test]
    fn slow_never_ticked_loop_is_judged_by_its_own_interval() {
        let health = Arc::new(BackgroundLoopHealth::new());
        health
            .loops
            .lock()
            .expect("loop health mutex poisoned")
            .insert(
                "slow-ghost-loop",
                LoopStatus {
                    tick_count: 0,
                    failure_count: 0,
                    last_tick_at: None,
                    last_success_at: None,
                    last_error: None,
                    expected_interval_secs: 3600,
                    registered_at: Instant::now()
                        .checked_sub(Duration::from_secs(120))
                        .expect("clock moved backwards"),
                },
            );
        // Registered 120s ago with a 1h cadence (threshold 5400s): still
        // inside its own window, so it must not report stale.
        assert!(!health.snapshot(60)[0].stale);
    }

    #[test]
    fn staleness_threshold_saturates_on_absurd_intervals() {
        assert_eq!(
            BackgroundLoopHealth::stale_threshold_secs(60, u64::MAX),
            u64::MAX
        );
    }

    #[test]
    fn interval_hint_raises_staleness_threshold_for_slow_loops() {
        let health = Arc::new(BackgroundLoopHealth::new());
        let fast = health.register_loop_with_interval("fast-loop", 2);
        let slow = health.register_loop_with_interval("slow-loop", 3600);
        fast.tick_ok();
        slow.tick_ok();
        let snapshot = health.snapshot(60);
        let fast_status = snapshot.iter().find(|s| s.name == "fast-loop").unwrap();
        let slow_status = snapshot.iter().find(|s| s.name == "slow-loop").unwrap();
        // The fast loop is judged by the global floor (60s here).
        assert!(!fast_status.stale);
        // The slow loop just ticked, so it is healthy even though its own
        // cadence (3600s → 5400s threshold) dwarfs the floor.
        assert!(!slow_status.stale);
    }

    #[test]
    fn set_loop_interval_updates_threshold_in_place() {
        let health = Arc::new(BackgroundLoopHealth::new());
        let handle = health.register_loop("rescheduled-loop");
        handle.tick_ok();
        // Without a hint the global floor governs; raising it to 100s makes
        // the loop's threshold max(60, 150) = 150s without re-registering.
        health.set_loop_interval("rescheduled-loop", 100);
        let snapshot = health.snapshot(60);
        assert_eq!(snapshot.len(), 1, "interval update must not duplicate");
        assert!(!snapshot[0].stale);
    }

    #[test]
    fn future_tick_instants_do_not_panic_snapshotting() {
        let health = Arc::new(BackgroundLoopHealth::new());
        health.register_loop("future-loop");
        health
            .loops
            .lock()
            .expect("loop health mutex poisoned")
            .get_mut("future-loop")
            .expect("registered loop")
            .last_tick_at = Some(Instant::now() + Duration::from_secs(1));

        let snapshot = health.snapshot(60);
        assert_eq!(snapshot[0].last_tick_secs_ago, Some(0));
        assert!(!snapshot[0].stale);
    }
}
