use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Read-only snapshot of [`SignalRateLimiter`] state.
pub struct SignalLimiterSnapshot {
    pub tracked_sources: usize,
    pub limit_per_minute: u32,
}

/// Read-only snapshot of [`PasswordResetRateLimiter`] state.
pub struct PasswordResetLimiterSnapshot {
    pub tracked_identifiers: usize,
    pub limit_per_hour: usize,
}

struct PasswordResetState {
    timestamps: HashMap<String, VecDeque<Instant>>,
    expirations: VecDeque<(Instant, String)>,
    active_identifiers: usize,
}

/// Per-identifier rate limiter for `POST /auth/reset-password`.
///
/// Uses a 1-hour rolling window per email address to prevent brute-force
/// and enumeration attacks on the password reset flow.
///
/// Memory is bounded: at most `max_tracked_keys` identifiers are tracked at
/// once; expired timestamps are evicted lazily on each access.
pub struct PasswordResetRateLimiter {
    state: Mutex<PasswordResetState>,
    max_per_hour: usize,
    max_tracked_keys: usize,
}

impl PasswordResetRateLimiter {
    const WINDOW: Duration = Duration::from_secs(3600);

    fn prune_identifier_timestamps(timestamps: &mut VecDeque<Instant>, now: Instant) {
        while let Some(&front) = timestamps.front() {
            if now.duration_since(front) >= Self::WINDOW {
                timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn prune_expired_timestamps(state: &mut PasswordResetState, now: Instant) {
        while state
            .expirations
            .front()
            .is_some_and(|(expires_at, _)| *expires_at <= now)
        {
            let (_, identifier) = state.expirations.pop_front().expect("front checked above");
            let should_remove = match state.timestamps.get_mut(&identifier) {
                Some(timestamps) => {
                    Self::prune_identifier_timestamps(timestamps, now);
                    timestamps.is_empty()
                }
                None => false,
            };
            if should_remove {
                state.timestamps.remove(&identifier);
                state.active_identifiers = state.active_identifiers.saturating_sub(1);
            }
        }
    }

    #[cfg(test)]
    fn insert_for_test(&self, identifier: &str, timestamps: VecDeque<Instant>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !timestamps.is_empty() {
            state.active_identifiers += 1;
        }
        state.expirations.extend(
            timestamps
                .iter()
                .copied()
                .map(|timestamp| (timestamp + Self::WINDOW, identifier.to_string())),
        );
        state
            .expirations
            .make_contiguous()
            .sort_by_key(|(expires_at, _)| *expires_at);
        state.timestamps.insert(identifier.to_string(), timestamps);
    }

    #[cfg(test)]
    fn window() -> Duration {
        Self::WINDOW
    }

    #[cfg(test)]
    fn len_for_test(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .timestamps
            .len()
    }

    #[cfg(test)]
    fn contains_for_test(&self, identifier: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .timestamps
            .contains_key(identifier)
    }

    pub fn new(max_per_hour: u32) -> Self {
        Self {
            state: Mutex::new(PasswordResetState {
                timestamps: HashMap::new(),
                expirations: VecDeque::new(),
                active_identifiers: 0,
            }),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys: 100_000,
        }
    }

    #[cfg(test)]
    fn new_with_cap(max_per_hour: u32, max_tracked_keys: usize) -> Self {
        Self {
            state: Mutex::new(PasswordResetState {
                timestamps: HashMap::new(),
                expirations: VecDeque::new(),
                active_identifiers: 0,
            }),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys,
        }
    }

    /// Return a read-only snapshot of the current limiter state.
    pub fn snapshot(&self) -> PasswordResetLimiterSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune_expired_timestamps(&mut state, Instant::now());
        PasswordResetLimiterSnapshot {
            tracked_identifiers: state.active_identifiers,
            limit_per_hour: self.max_per_hour,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, identifier: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();

        Self::prune_expired_timestamps(&mut state, now);

        let key = identifier.to_string();
        let is_new_identifier = !state.timestamps.contains_key(&key);
        if is_new_identifier && state.timestamps.len() >= self.max_tracked_keys {
            return false;
        }

        if is_new_identifier {
            state.timestamps.insert(key.clone(), VecDeque::new());
            state.active_identifiers += 1;
        }

        let entry = state
            .timestamps
            .get_mut(&key)
            .expect("identifier inserted above");
        if entry.len() >= self.max_per_hour {
            return false;
        }

        entry.push_back(now);
        state.expirations.push_back((now + Self::WINDOW, key));
        true
    }
}

/// Per-source rate limiter for `POST /signals` ingestion.
pub struct SignalRateLimiter {
    state: Mutex<SignalRateLimiterState>,
    max_per_minute: u32,
}

struct SignalRateLimiterState {
    counts: HashMap<String, (u32, Instant)>,
    expirations: VecDeque<(Instant, String)>,
    active_sources: usize,
}

impl SignalRateLimiter {
    const WINDOW: Duration = Duration::from_secs(60);

    fn prune_expired_counts(state: &mut SignalRateLimiterState, now: Instant) {
        while state
            .expirations
            .front()
            .is_some_and(|(expires_at, _)| *expires_at <= now)
        {
            let (_, source) = state.expirations.pop_front().expect("front checked above");
            let should_remove = state
                .counts
                .get(&source)
                .is_some_and(|(_, window_start)| *window_start + Self::WINDOW <= now);
            if should_remove {
                state.counts.remove(&source);
                state.active_sources = state.active_sources.saturating_sub(1);
            }
        }
    }

    #[cfg(test)]
    fn insert_for_test(&self, source: &str, count: u32, window_start: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active_sources += 1;
        state
            .expirations
            .push_back((window_start + Self::WINDOW, source.to_string()));
        state
            .expirations
            .make_contiguous()
            .sort_by_key(|(expires_at, _)| *expires_at);
        state
            .counts
            .insert(source.to_string(), (count, window_start));
    }

    #[cfg(test)]
    fn contains_for_test(&self, source: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .counts
            .contains_key(source)
    }

    pub fn new(max_per_minute: u32) -> Self {
        Self {
            state: Mutex::new(SignalRateLimiterState {
                counts: HashMap::new(),
                expirations: VecDeque::new(),
                active_sources: 0,
            }),
            max_per_minute,
        }
    }

    /// Return a read-only snapshot of the current limiter state.
    pub fn snapshot(&self) -> SignalLimiterSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        Self::prune_expired_counts(&mut state, Instant::now());
        SignalLimiterSnapshot {
            tracked_sources: state.active_sources,
            limit_per_minute: self.max_per_minute,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, source: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();

        Self::prune_expired_counts(&mut state, now);

        if let Some((count, window_start)) = state.counts.get_mut(source) {
            if now.duration_since(*window_start) >= Self::WINDOW {
                *count = 1;
                *window_start = now;
                state
                    .expirations
                    .push_back((now + Self::WINDOW, source.to_string()));
                true
            } else if *count < self.max_per_minute {
                *count += 1;
                true
            } else {
                false
            }
        } else {
            state.counts.insert(source.to_string(), (1, now));
            state.active_sources += 1;
            state
                .expirations
                .push_back((now + Self::WINDOW, source.to_string()));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PasswordResetRateLimiter, SignalRateLimiter};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    #[test]
    fn allows_requests_within_limit() {
        let limiter = PasswordResetRateLimiter::new(3);
        assert!(limiter.check_and_increment("user@example.com"));
        assert!(limiter.check_and_increment("user@example.com"));
        assert!(limiter.check_and_increment("user@example.com"));
    }

    #[test]
    fn blocks_after_limit_exceeded() {
        let limiter = PasswordResetRateLimiter::new(2);
        assert!(limiter.check_and_increment("a@example.com"));
        assert!(limiter.check_and_increment("a@example.com"));
        assert!(!limiter.check_and_increment("a@example.com"));
    }

    #[test]
    fn limits_are_per_identifier() {
        let limiter = PasswordResetRateLimiter::new(1);
        assert!(limiter.check_and_increment("alice@example.com"));
        assert!(!limiter.check_and_increment("alice@example.com"));
        assert!(limiter.check_and_increment("bob@example.com"));
    }

    #[test]
    fn rejects_new_identifiers_when_key_cap_reached() {
        let limiter = PasswordResetRateLimiter::new_with_cap(10, 2);
        assert!(limiter.check_and_increment("a@example.com"));
        assert!(limiter.check_and_increment("b@example.com"));
        assert!(!limiter.check_and_increment("c@example.com"));
        assert!(limiter.check_and_increment("a@example.com"));
    }

    #[test]
    fn signal_snapshot_empty_on_fresh_limiter() {
        let limiter = SignalRateLimiter::new(60);
        let snap = limiter.snapshot();
        assert_eq!(snap.tracked_sources, 0);
        assert_eq!(snap.limit_per_minute, 60);
    }

    #[test]
    fn signal_snapshot_tracks_after_increment() {
        let limiter = SignalRateLimiter::new(60);
        limiter.check_and_increment("src1");
        let snap = limiter.snapshot();
        assert_eq!(snap.tracked_sources, 1);
    }

    #[test]
    fn signal_snapshot_is_read_only() {
        let limiter = SignalRateLimiter::new(60);
        limiter.check_and_increment("src1");
        let snap1 = limiter.snapshot();
        let snap2 = limiter.snapshot();
        assert_eq!(snap1.tracked_sources, snap2.tracked_sources);
    }

    #[test]
    fn signal_snapshot_excludes_expired_sources() {
        let limiter = SignalRateLimiter::new(60);
        let now = Instant::now();
        limiter.insert_for_test("active", 1, now - Duration::from_secs(5));
        limiter.insert_for_test("expired", 1, now - Duration::from_secs(61));

        let snap = limiter.snapshot();

        assert_eq!(snap.tracked_sources, 1);
    }

    #[test]
    fn signal_hot_path_only_prunes_requested_source() {
        let limiter = SignalRateLimiter::new(60);
        let now = Instant::now();
        limiter.insert_for_test("active", 1, now - Duration::from_secs(5));
        limiter.insert_for_test("expired", 1, now - Duration::from_secs(61));

        assert!(limiter.check_and_increment("active"));

        assert!(!limiter.contains_for_test("expired"));
        assert!(limiter.contains_for_test("active"));
    }

    #[test]
    fn password_reset_snapshot_excludes_expired_identifiers() {
        let limiter = PasswordResetRateLimiter::new(5);
        let now = Instant::now();
        limiter.insert_for_test(
            "active@example.com",
            VecDeque::from([now - Duration::from_secs(30)]),
        );
        limiter.insert_for_test(
            "expired@example.com",
            VecDeque::from([now - (PasswordResetRateLimiter::window() + Duration::from_secs(1))]),
        );

        let snap = limiter.snapshot();

        assert_eq!(snap.tracked_identifiers, 1);
    }

    #[test]
    fn password_reset_prunes_expired_identifiers_before_capacity_check() {
        let limiter = PasswordResetRateLimiter::new_with_cap(10, 1);
        let expired =
            Instant::now() - (PasswordResetRateLimiter::window() + Duration::from_secs(1));
        limiter.insert_for_test("expired@example.com", VecDeque::from([expired]));

        assert!(limiter.check_and_increment("fresh@example.com"));
        assert_eq!(limiter.len_for_test(), 1);
        assert!(limiter.contains_for_test("fresh@example.com"));
    }

    #[test]
    fn password_reset_hot_path_only_prunes_requested_identifier() {
        let limiter = PasswordResetRateLimiter::new_with_cap(10, 10);
        let now = Instant::now();
        let expired = now - (PasswordResetRateLimiter::window() + Duration::from_secs(1));

        limiter.insert_for_test(
            "active@example.com",
            VecDeque::from([now - Duration::from_secs(5)]),
        );
        limiter.insert_for_test("expired@example.com", VecDeque::from([expired]));

        assert!(limiter.check_and_increment("active@example.com"));
        assert_eq!(limiter.len_for_test(), 1);
        assert!(!limiter.contains_for_test("expired@example.com"));
    }
}
