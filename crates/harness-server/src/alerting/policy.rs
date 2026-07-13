//! Alert policy: class allowlist filter (B-003) and dedup cooldown (B-010).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use harness_core::alert::{AlertClass, AlertPayload};
use tokio::time::Instant;

#[derive(Debug, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Deliver; carries the number of alerts suppressed for this dedup key
    /// since the previous delivery (audited per B-010).
    Deliver { suppressed_in_window: u64 },
    /// Class not in the configured allowlist.
    FilteredClass,
    /// Within the cooldown window for this dedup key.
    Suppressed,
}

pub struct AlertPolicy {
    allowed: HashSet<AlertClass>,
    default_cooldown: Duration,
    last_sent: HashMap<String, Instant>,
    suppressed: HashMap<String, u64>,
}

impl AlertPolicy {
    pub fn new(allowed: impl IntoIterator<Item = AlertClass>, default_cooldown: Duration) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
            default_cooldown,
            last_sent: HashMap::new(),
            suppressed: HashMap::new(),
        }
    }

    /// Evaluate one payload. `cooldown_override` lets producers carry their
    /// own window (e.g. `ready_to_merge_alert_ttl_secs`, B-019).
    pub fn evaluate(
        &mut self,
        payload: &AlertPayload,
        cooldown_override: Option<Duration>,
        now: Instant,
    ) -> PolicyDecision {
        if !self.allowed.contains(&payload.event_class) {
            return PolicyDecision::FilteredClass;
        }
        let cooldown = cooldown_override.unwrap_or(self.default_cooldown);
        if let Some(last) = self.last_sent.get(&payload.dedup_key) {
            if now.saturating_duration_since(*last) < cooldown {
                *self
                    .suppressed
                    .entry(payload.dedup_key.clone())
                    .or_default() += 1;
                return PolicyDecision::Suppressed;
            }
        }
        self.last_sent.insert(payload.dedup_key.clone(), now);
        let suppressed_in_window = self.suppressed.remove(&payload.dedup_key).unwrap_or(0);
        PolicyDecision::Deliver {
            suppressed_in_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::alert::AlertSeverity;

    fn payload(class: AlertClass, dedup: &str) -> AlertPayload {
        AlertPayload::new(class, AlertSeverity::Error, "msg", dedup)
    }

    #[tokio::test(start_paused = true)]
    async fn filters_class_not_in_allowlist() {
        let mut policy = AlertPolicy::new([AlertClass::WorkflowBlocked], Duration::from_secs(300));
        let decision = policy.evaluate(
            &payload(AlertClass::TaskFailureExhausted, "k"),
            None,
            Instant::now(),
        );
        assert_eq!(decision, PolicyDecision::FilteredClass);
    }

    #[tokio::test(start_paused = true)]
    async fn suppresses_within_cooldown_and_counts() {
        let mut policy =
            AlertPolicy::new([AlertClass::CircuitBreakerOpen], Duration::from_secs(300));
        let p = payload(AlertClass::CircuitBreakerOpen, "circuit_breaker_open:x");

        let first = policy.evaluate(&p, None, Instant::now());
        assert_eq!(
            first,
            PolicyDecision::Deliver {
                suppressed_in_window: 0
            }
        );

        // Two repeats inside the window are suppressed.
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(
            policy.evaluate(&p, None, Instant::now()),
            PolicyDecision::Suppressed
        );
        tokio::time::advance(Duration::from_secs(100)).await;
        assert_eq!(
            policy.evaluate(&p, None, Instant::now()),
            PolicyDecision::Suppressed
        );

        // Past the window: delivered again, suppression count reported.
        tokio::time::advance(Duration::from_secs(101)).await;
        assert_eq!(
            policy.evaluate(&p, None, Instant::now()),
            PolicyDecision::Deliver {
                suppressed_in_window: 2
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn distinct_dedup_keys_do_not_interfere() {
        let mut policy = AlertPolicy::new([AlertClass::WorkflowFailed], Duration::from_secs(300));
        let a = payload(AlertClass::WorkflowFailed, "workflow_failed:a");
        let b = payload(AlertClass::WorkflowFailed, "workflow_failed:b");
        assert!(matches!(
            policy.evaluate(&a, None, Instant::now()),
            PolicyDecision::Deliver { .. }
        ));
        assert!(matches!(
            policy.evaluate(&b, None, Instant::now()),
            PolicyDecision::Deliver { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_override_takes_precedence() {
        let mut policy = AlertPolicy::new([AlertClass::ReadyToMergeAging], Duration::from_secs(10));
        let p = payload(AlertClass::ReadyToMergeAging, "ready_to_merge_aging:pr-1");
        let ttl = Some(Duration::from_secs(3600));

        assert!(matches!(
            policy.evaluate(&p, ttl, Instant::now()),
            PolicyDecision::Deliver { .. }
        ));
        // Past the default cooldown but inside the override window.
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(
            policy.evaluate(&p, ttl, Instant::now()),
            PolicyDecision::Suppressed
        );
        // Past the override window.
        tokio::time::advance(Duration::from_secs(3001)).await;
        assert!(matches!(
            policy.evaluate(&p, ttl, Instant::now()),
            PolicyDecision::Deliver {
                suppressed_in_window: 1
            }
        ));
    }
}
