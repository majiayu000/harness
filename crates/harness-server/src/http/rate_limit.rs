use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

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

/// Per-identifier rate limiter for `POST /auth/reset-password`.
///
/// Uses a 1-hour rolling window per email address to prevent brute-force
/// and enumeration attacks on the password reset flow.
///
/// Memory is bounded: at most `max_tracked_keys` identifiers are tracked at
/// once; expired timestamps are evicted lazily on each access.
pub struct PasswordResetRateLimiter {
    timestamps: Mutex<HashMap<String, VecDeque<Instant>>>,
    max_per_hour: usize,
    max_tracked_keys: usize,
}

impl PasswordResetRateLimiter {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

    pub fn new(max_per_hour: u32) -> Self {
        Self {
            timestamps: Mutex::new(HashMap::new()),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys: 100_000,
        }
    }

    #[cfg(test)]
    fn new_with_cap(max_per_hour: u32, max_tracked_keys: usize) -> Self {
        Self {
            timestamps: Mutex::new(HashMap::new()),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys,
        }
    }

    /// Return a read-only snapshot of the current limiter state.
    pub fn snapshot(&self) -> PasswordResetLimiterSnapshot {
        let map = self.timestamps.lock().unwrap_or_else(|p| p.into_inner());
        PasswordResetLimiterSnapshot {
            tracked_identifiers: map.len(),
            limit_per_hour: self.max_per_hour,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, identifier: &str) -> bool {
        let mut map = self.timestamps.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();

        // Evict timestamps outside the rolling window for this identifier.
        if let Some(entry) = map.get_mut(identifier) {
            while let Some(&front) = entry.front() {
                if now.duration_since(front) >= Self::WINDOW {
                    entry.pop_front();
                } else {
                    break;
                }
            }
            if entry.is_empty() {
                map.remove(identifier);
            }
        }

        // Reject new identifiers when map is at capacity (memory-DoS guard).
        if !map.contains_key(identifier) && map.len() >= self.max_tracked_keys {
            return false;
        }

        let entry = map.entry(identifier.to_string()).or_default();
        if entry.len() < self.max_per_hour {
            entry.push_back(now);
            true
        } else {
            false
        }
    }
}

/// Per-source rate limiter for `POST /signals` ingestion.
pub struct SignalRateLimiter {
    counts: Mutex<HashMap<String, (u32, Instant)>>,
    max_per_minute: u32,
}

impl SignalRateLimiter {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
            max_per_minute,
        }
    }

    /// Return a read-only snapshot of the current limiter state.
    pub fn snapshot(&self) -> SignalLimiterSnapshot {
        let counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        SignalLimiterSnapshot {
            tracked_sources: counts.len(),
            limit_per_minute: self.max_per_minute,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, source: &str) -> bool {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let entry = counts.entry(source.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) >= std::time::Duration::from_secs(60) {
            *entry = (1, now);
            true
        } else if entry.0 < self.max_per_minute {
            entry.0 += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PasswordResetRateLimiter, SignalRateLimiter};

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
}
