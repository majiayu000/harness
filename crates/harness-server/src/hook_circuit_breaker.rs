use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum consecutive blocks before the circuit opens.
const FAILURE_THRESHOLD: u32 = 3;
/// How long the circuit stays open (auto-pass) before transitioning to half-open.
pub const COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Debug)]
enum State {
    /// Normal operation — enforcement runs on every call.
    Closed { consecutive_failures: u32 },
    /// Circuit tripped — calls auto-pass until cooldown expires.
    Open { opened_at: Instant },
    /// Cooldown expired — allow one probe call to test recovery.
    HalfOpen,
}

struct Entry {
    state: State,
}

impl Entry {
    fn new() -> Self {
        Self {
            state: State::Closed {
                consecutive_failures: 0,
            },
        }
    }
}

/// Martin Fowler–style circuit breaker for hook enforcement.
///
/// Tracks consecutive block counts per key (typically a session ID or hook
/// name). After [`FAILURE_THRESHOLD`] consecutive blocks the circuit opens and
/// all subsequent calls auto-pass for [`COOLDOWN`] seconds, preventing infinite
/// loops caused by unresolvable violations (GitHub issue #3573, #10205).
///
/// This replaces the role of Claude Code's `stop_hook_active` field: when the
/// circuit is open the enforcer behaves as if `stop_hook_active = true` — it
/// does not emit a blocking signal, breaking the loop.
///
/// State machine:
/// ```text
/// CLOSED ──(≥3 blocks)──► OPEN ──(5 min elapsed)──► HALF-OPEN
///   ▲                                                     │
///   └─────────────(pass)─────────────────────────────────┘
///                         OPEN ◄──(block)────────── HALF-OPEN
/// ```
pub struct CircuitBreaker {
    entries: Mutex<HashMap<String, Entry>>,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Returns `true` if enforcement should proceed.
    /// Returns `false` if the circuit is open and still in cooldown (auto-pass).
    pub fn allow(&self, key: &str) -> bool {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_insert_with(Entry::new);
        match &entry.state {
            State::Closed { .. } => true,
            State::Open { opened_at } => {
                if opened_at.elapsed() >= COOLDOWN {
                    tracing::info!(
                        key,
                        "hook_circuit_breaker: cooldown elapsed, transitioning to half-open"
                    );
                    entry.state = State::HalfOpen;
                    true
                } else {
                    false
                }
            }
            State::HalfOpen => true,
        }
    }

    /// Record a block outcome (violations found). Opens the circuit after
    /// [`FAILURE_THRESHOLD`] consecutive blocks.
    pub fn record_block(&self, key: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_insert_with(Entry::new);
        match &entry.state {
            State::Closed {
                consecutive_failures,
            } => {
                let next = consecutive_failures + 1;
                if next >= FAILURE_THRESHOLD {
                    tracing::warn!(
                        key,
                        consecutive = next,
                        "hook_circuit_breaker: opening circuit after {} consecutive blocks",
                        next
                    );
                    entry.state = State::Open {
                        opened_at: Instant::now(),
                    };
                } else {
                    entry.state = State::Closed {
                        consecutive_failures: next,
                    };
                }
            }
            State::HalfOpen => {
                tracing::warn!(
                    key,
                    "hook_circuit_breaker: probe failed, re-opening circuit"
                );
                entry.state = State::Open {
                    opened_at: Instant::now(),
                };
            }
            State::Open { .. } => {}
        }
    }

    /// Record a pass outcome (no violations). Resets the failure counter and
    /// closes the circuit if it was half-open.
    pub fn record_pass(&self, key: &str) {
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_insert_with(Entry::new);
        if matches!(entry.state, State::HalfOpen) {
            tracing::info!(key, "hook_circuit_breaker: probe passed, closing circuit");
        }
        entry.state = State::Closed {
            consecutive_failures: 0,
        };
    }

    /// Returns `true` when the circuit for `key` is currently open (auto-passing).
    #[cfg(test)]
    pub fn is_open(&self, key: &str) -> bool {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        matches!(map.get(key).map(|e| &e.state), Some(State::Open { .. }))
    }

    /// Returns the consecutive failure count for `key` when the circuit is closed.
    #[cfg(test)]
    pub fn consecutive_failures(&self, key: &str) -> u32 {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key).map(|e| &e.state) {
            Some(State::Closed {
                consecutive_failures,
            }) => *consecutive_failures,
            _ => 0,
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_starts_closed_and_allows() {
        let cb = CircuitBreaker::new();
        assert!(cb.allow("sess-1"));
    }

    #[test]
    fn opens_after_failure_threshold() {
        let cb = CircuitBreaker::new();
        let key = "sess-open";
        for _ in 0..FAILURE_THRESHOLD {
            assert!(cb.allow(key));
            cb.record_block(key);
        }
        assert!(
            cb.is_open(key),
            "circuit must be open after threshold blocks"
        );
        assert!(!cb.allow(key), "open circuit must deny enforcement");
    }

    #[test]
    fn pass_resets_consecutive_counter() {
        let cb = CircuitBreaker::new();
        let key = "sess-reset";
        cb.record_block(key);
        cb.record_block(key);
        assert_eq!(cb.consecutive_failures(key), 2);
        cb.record_pass(key);
        assert_eq!(cb.consecutive_failures(key), 0);
        assert!(!cb.is_open(key));
    }

    #[test]
    fn below_threshold_stays_closed() {
        let cb = CircuitBreaker::new();
        let key = "sess-below";
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            cb.record_block(key);
        }
        assert!(!cb.is_open(key));
        assert!(cb.allow(key));
    }

    #[test]
    fn open_circuit_auto_passes() {
        let cb = CircuitBreaker::new();
        let key = "sess-autopass";
        for _ in 0..FAILURE_THRESHOLD {
            cb.record_block(key);
        }
        // Circuit is open — allow() returns false (caller skips enforcement,
        // which means auto-pass for the agent).
        assert!(!cb.allow(key));
    }
}
