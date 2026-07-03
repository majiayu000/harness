use chrono::{DateTime, Duration, Utc};
use harness_core::config::workflow_circuit_breaker::RuntimeCircuitBreakerPolicy;
use harness_workflow::runtime::{RuntimeJob, RuntimeJobClaimDecision, RuntimeJobClaimGuard};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FailureClass {
    QuotaInteractiveWait,
    CliMissingFile,
    WorktreeCollision,
    StructuredOutputMissing,
    SandboxPermission,
    Unclassified,
}

impl FailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::QuotaInteractiveWait => "quota-interactive-wait",
            Self::CliMissingFile => "cli-missing-file",
            Self::WorktreeCollision => "worktree-collision",
            Self::StructuredOutputMissing => "structured-output-missing",
            Self::SandboxPermission => "sandbox-permission",
            Self::Unclassified => "unclassified",
        }
    }
}

pub(crate) fn classify_agent_failure(error: &str) -> FailureClass {
    let lower = error.to_ascii_lowercase();
    if error.contains("Reading additional input")
        || lower.contains("hit your limit")
        || lower.contains("hit your usage limit")
    {
        return FailureClass::QuotaInteractiveWait;
    }
    if error.contains("No such file or directory") {
        return FailureClass::CliMissingFile;
    }
    if error.contains("WorktreeCollision") {
        return FailureClass::WorktreeCollision;
    }
    if lower.contains("no harness-activity-result") {
        return FailureClass::StructuredOutputMissing;
    }
    if lower.contains("sandbox") || lower.contains("permission denied") {
        return FailureClass::SandboxPermission;
    }
    FailureClass::Unclassified
}

#[derive(Debug, Clone)]
pub(crate) struct DeferredProfile {
    pub profile: String,
    pub until: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct CircuitBreakerSnapshot {
    pub profile: String,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consecutive: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CircuitBreakerEventKind {
    Opened,
    Closed,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CircuitBreakerEvent {
    pub kind: CircuitBreakerEventKind,
    pub profile: String,
    pub class: Option<FailureClass>,
    pub consecutive: Option<u32>,
    pub cooldown_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
enum ProfileBreaker {
    Closed {
        consecutive: BTreeMap<FailureClass, u32>,
    },
    Open {
        class: FailureClass,
        until: DateTime<Utc>,
        backoff_exp: u32,
        consecutive: u32,
    },
    HalfOpen {
        class: FailureClass,
        backoff_exp: u32,
        probe_runtime_job_id: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct RuntimeCircuitBreakerRegistry {
    config: RuntimeCircuitBreakerPolicy,
    inner: Mutex<BTreeMap<String, ProfileBreaker>>,
}

impl RuntimeCircuitBreakerRegistry {
    pub(crate) fn new(config: RuntimeCircuitBreakerPolicy) -> Self {
        Self {
            config,
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn defer_open_profiles(&self, now: DateTime<Utc>) -> Vec<DeferredProfile> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut deferred = Vec::new();
        for (profile, state) in inner.iter_mut() {
            match state {
                ProfileBreaker::Open {
                    class,
                    until,
                    backoff_exp,
                    ..
                } if *until <= now => {
                    *state = ProfileBreaker::HalfOpen {
                        class: *class,
                        backoff_exp: *backoff_exp,
                        probe_runtime_job_id: None,
                    };
                }
                ProfileBreaker::Open { until, .. } => deferred.push(DeferredProfile {
                    profile: profile.clone(),
                    until: *until,
                }),
                ProfileBreaker::Closed { .. } | ProfileBreaker::HalfOpen { .. } => {}
            }
        }
        deferred
    }

    pub(crate) fn record_success(
        &self,
        profile: &str,
        runtime_job_id: &str,
        now: DateTime<Utc>,
    ) -> Vec<CircuitBreakerEvent> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(state) = inner.get_mut(profile) else {
            return Vec::new();
        };
        match state {
            ProfileBreaker::Closed { consecutive } => {
                consecutive.clear();
                Vec::new()
            }
            ProfileBreaker::Open { .. } => Vec::new(),
            ProfileBreaker::HalfOpen {
                class,
                probe_runtime_job_id,
                ..
            } if probe_runtime_job_id.as_deref() == Some(runtime_job_id) => {
                let class = *class;
                *state = ProfileBreaker::Closed {
                    consecutive: BTreeMap::new(),
                };
                vec![CircuitBreakerEvent {
                    kind: CircuitBreakerEventKind::Closed,
                    profile: profile.to_string(),
                    class: Some(class),
                    consecutive: None,
                    cooldown_until: Some(now),
                }]
            }
            ProfileBreaker::HalfOpen { .. } => Vec::new(),
        }
    }

    pub(crate) fn record_failure(
        &self,
        profile: &str,
        runtime_job_id: &str,
        class: FailureClass,
        now: DateTime<Utc>,
    ) -> Vec<CircuitBreakerEvent> {
        if !self.config.enabled {
            return Vec::new();
        }
        let config = self.config.clone();
        let threshold = config.consecutive_failures.max(1);
        let mut inner = self.inner.lock().expect("runtime breaker mutex poisoned");
        let state = inner
            .entry(profile.to_string())
            .or_insert_with(|| ProfileBreaker::Closed {
                consecutive: BTreeMap::new(),
            });
        match state {
            ProfileBreaker::Closed { consecutive } => {
                let count = consecutive.entry(class).or_insert(0);
                *count = count.saturating_add(1);
                if *count < threshold {
                    return Vec::new();
                }
                let until = now + cooldown_duration_for_config(&config, 0);
                let consecutive_count = *count;
                *state = ProfileBreaker::Open {
                    class,
                    until,
                    backoff_exp: 0,
                    consecutive: consecutive_count,
                };
                vec![CircuitBreakerEvent {
                    kind: CircuitBreakerEventKind::Opened,
                    profile: profile.to_string(),
                    class: Some(class),
                    consecutive: Some(consecutive_count),
                    cooldown_until: Some(until),
                }]
            }
            ProfileBreaker::HalfOpen {
                backoff_exp,
                probe_runtime_job_id,
                ..
            } if probe_runtime_job_id.as_deref() == Some(runtime_job_id) => {
                let next_exp = backoff_exp.saturating_add(1);
                let until = now + cooldown_duration_for_config(&config, next_exp);
                *state = ProfileBreaker::Open {
                    class,
                    until,
                    backoff_exp: next_exp,
                    consecutive: threshold,
                };
                vec![CircuitBreakerEvent {
                    kind: CircuitBreakerEventKind::Opened,
                    profile: profile.to_string(),
                    class: Some(class),
                    consecutive: Some(threshold),
                    cooldown_until: Some(until),
                }]
            }
            ProfileBreaker::HalfOpen { .. } => Vec::new(),
            ProfileBreaker::Open { .. } => Vec::new(),
        }
    }

    pub(crate) fn reset(&self, profile: &str, now: DateTime<Utc>) -> CircuitBreakerEvent {
        let mut inner = self.inner.lock().expect("runtime breaker mutex poisoned");
        inner.insert(
            profile.to_string(),
            ProfileBreaker::Closed {
                consecutive: BTreeMap::new(),
            },
        );
        CircuitBreakerEvent {
            kind: CircuitBreakerEventKind::Reset,
            profile: profile.to_string(),
            class: None,
            consecutive: None,
            cooldown_until: Some(now),
        }
    }

    pub(crate) fn snapshots(&self, now: DateTime<Utc>) -> Vec<CircuitBreakerSnapshot> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut inner = self.inner.lock().expect("runtime breaker mutex poisoned");
        let mut snapshots = Vec::new();
        for (profile, state) in inner.iter_mut() {
            if let ProfileBreaker::Open {
                class,
                until,
                backoff_exp,
                ..
            } = state
            {
                if *until <= now {
                    *state = ProfileBreaker::HalfOpen {
                        class: *class,
                        backoff_exp: *backoff_exp,
                        probe_runtime_job_id: None,
                    };
                }
            }
            match state {
                ProfileBreaker::Open {
                    class,
                    until,
                    consecutive,
                    ..
                } => snapshots.push(CircuitBreakerSnapshot {
                    profile: profile.clone(),
                    state: "open",
                    class: Some(class.as_str().to_string()),
                    consecutive: Some(*consecutive),
                    cooldown_until: Some(*until),
                }),
                ProfileBreaker::HalfOpen { class, .. } => snapshots.push(CircuitBreakerSnapshot {
                    profile: profile.clone(),
                    state: "half_open",
                    class: Some(class.as_str().to_string()),
                    consecutive: None,
                    cooldown_until: None,
                }),
                ProfileBreaker::Closed { consecutive } if !consecutive.is_empty() => {
                    let (class, count) = consecutive
                        .iter()
                        .max_by_key(|(_, count)| *count)
                        .expect("non-empty consecutive map has max");
                    snapshots.push(CircuitBreakerSnapshot {
                        profile: profile.clone(),
                        state: "closed",
                        class: Some(class.as_str().to_string()),
                        consecutive: Some(*count),
                        cooldown_until: None,
                    });
                }
                ProfileBreaker::Closed { .. } => {}
            }
        }
        snapshots
    }
}

impl RuntimeJobClaimGuard for RuntimeCircuitBreakerRegistry {
    fn before_execute(
        &self,
        job: &RuntimeJob,
        now: DateTime<Utc>,
        _lease_expires_at: DateTime<Utc>,
    ) -> RuntimeJobClaimDecision {
        if !self.config.enabled {
            return RuntimeJobClaimDecision::Proceed;
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(state) = inner.get_mut(&job.runtime_profile) else {
            return RuntimeJobClaimDecision::Proceed;
        };
        match state {
            ProfileBreaker::Closed { .. } => RuntimeJobClaimDecision::Proceed,
            ProfileBreaker::Open {
                class,
                until,
                backoff_exp,
                ..
            } if *until <= now => {
                *state = ProfileBreaker::HalfOpen {
                    class: *class,
                    backoff_exp: *backoff_exp,
                    probe_runtime_job_id: Some(job.id.clone()),
                };
                RuntimeJobClaimDecision::Proceed
            }
            ProfileBreaker::Open { until, .. } => RuntimeJobClaimDecision::Defer {
                not_before: *until,
                reason: "runtime circuit breaker open".to_string(),
            },
            ProfileBreaker::HalfOpen {
                probe_runtime_job_id,
                ..
            } => {
                if probe_runtime_job_id
                    .as_ref()
                    .is_some_and(|runtime_job_id| runtime_job_id != &job.id)
                {
                    return RuntimeJobClaimDecision::Defer {
                        not_before: now + Duration::seconds(1),
                        reason: "runtime circuit breaker half-open probe in flight".to_string(),
                    };
                }
                *probe_runtime_job_id = Some(job.id.clone());
                RuntimeJobClaimDecision::Proceed
            }
        }
    }
}

fn cooldown_duration_for_config(
    config: &RuntimeCircuitBreakerPolicy,
    backoff_exp: u32,
) -> Duration {
    let base = config.cooldown_secs.max(1) as f64;
    let factor = config.backoff_factor.max(1.0);
    let scaled = base * factor.powi(backoff_exp.min(16) as i32);
    let secs = scaled
        .round()
        .clamp(1.0, config.max_cooldown_secs.max(1) as f64) as i64;
    Duration::seconds(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry() -> RuntimeCircuitBreakerRegistry {
        RuntimeCircuitBreakerRegistry::new(RuntimeCircuitBreakerPolicy {
            enabled: true,
            consecutive_failures: 3,
            cooldown_secs: 10,
            backoff_factor: 2.0,
            max_cooldown_secs: 60,
        })
    }

    #[test]
    fn classify_agent_failure_maps_known_classes() {
        assert_eq!(
            classify_agent_failure(
                "codex exited with exit status: 1: stderr=[Reading additional input"
            ),
            FailureClass::QuotaInteractiveWait
        );
        assert_eq!(
            classify_agent_failure("agent hit your usage limit"),
            FailureClass::QuotaInteractiveWait
        );
        assert_eq!(
            classify_agent_failure("spawn failed: No such file or directory"),
            FailureClass::CliMissingFile
        );
        assert_eq!(
            classify_agent_failure("WorktreeCollision: already checked out"),
            FailureClass::WorktreeCollision
        );
        assert_eq!(
            classify_agent_failure("no harness-activity-result block found"),
            FailureClass::StructuredOutputMissing
        );
        assert_eq!(
            classify_agent_failure("sandbox denied: permission denied"),
            FailureClass::SandboxPermission
        );
        assert_eq!(
            classify_agent_failure("something new"),
            FailureClass::Unclassified
        );
    }

    #[test]
    fn breaker_opens_after_threshold_and_defers_profile() {
        let registry = test_registry();
        let now = Utc::now();

        assert!(registry
            .record_failure("codex", "job-1", FailureClass::QuotaInteractiveWait, now)
            .is_empty());
        assert!(registry
            .record_failure("codex", "job-2", FailureClass::QuotaInteractiveWait, now)
            .is_empty());
        let events =
            registry.record_failure("codex", "job-3", FailureClass::QuotaInteractiveWait, now);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, CircuitBreakerEventKind::Opened);
        let deferred = registry.defer_open_profiles(now);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].profile, "codex");
    }

    #[test]
    fn success_clears_closed_counts() {
        let registry = test_registry();
        let now = Utc::now();
        registry.record_failure("codex", "job-1", FailureClass::Unclassified, now);

        registry.record_success("codex", "job-2", now);

        let snapshots = registry.snapshots(now);
        assert!(snapshots.is_empty());
    }

    #[test]
    fn half_open_success_closes_and_failure_reopens_with_backoff() {
        let registry = test_registry();
        let now = Utc::now();
        for _ in 0..3 {
            registry.record_failure("codex", "failing-job", FailureClass::Unclassified, now);
        }

        let later = now + Duration::seconds(11);
        assert!(registry.defer_open_profiles(later).is_empty());
        assert_eq!(registry.snapshots(later)[0].state, "half_open");
        let probe = RuntimeJob::pending(
            "command-probe",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex",
            serde_json::json!({ "activity": "check" }),
        );
        assert_eq!(
            registry.before_execute(&probe, later, later + Duration::minutes(5)),
            RuntimeJobClaimDecision::Proceed
        );
        let events = registry.record_failure("codex", &probe.id, FailureClass::Unclassified, later);

        assert_eq!(events[0].kind, CircuitBreakerEventKind::Opened);
        assert_eq!(
            events[0].cooldown_until,
            Some(later + Duration::seconds(20))
        );
        assert_eq!(registry.snapshots(later)[0].state, "open");

        let after_backoff = later + Duration::seconds(21);
        registry.defer_open_profiles(after_backoff);
        let success_probe = RuntimeJob::pending(
            "command-success-probe",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex",
            serde_json::json!({ "activity": "check" }),
        );
        assert_eq!(
            registry.before_execute(
                &success_probe,
                after_backoff,
                after_backoff + Duration::minutes(5),
            ),
            RuntimeJobClaimDecision::Proceed
        );
        let events = registry.record_success("codex", &success_probe.id, after_backoff);
        assert_eq!(events[0].kind, CircuitBreakerEventKind::Closed);
        assert!(registry.snapshots(after_backoff).is_empty());
    }

    #[test]
    fn claim_guard_blocks_open_and_allows_one_half_open_probe() {
        let registry = test_registry();
        let now = Utc::now();
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex",
            serde_json::json!({ "activity": "check" }),
        );
        let second_job = RuntimeJob::pending(
            "command-2",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex",
            serde_json::json!({ "activity": "check" }),
        );
        for _ in 0..3 {
            registry.record_failure(
                "codex",
                "threshold-job",
                FailureClass::QuotaInteractiveWait,
                now,
            );
        }

        let open_decision = registry.before_execute(&job, now, now + Duration::minutes(5));
        match open_decision {
            RuntimeJobClaimDecision::Defer { not_before, .. } => {
                assert_eq!(not_before, now + Duration::seconds(10));
            }
            RuntimeJobClaimDecision::Proceed => {
                panic!("open circuit should defer runtime dispatch")
            }
        }
        assert!(registry
            .record_success("codex", "old-running-job", now)
            .is_empty());
        assert_eq!(registry.snapshots(now)[0].state, "open");

        let after_cooldown = now + Duration::seconds(11);
        assert_eq!(
            registry.before_execute(&job, after_cooldown, after_cooldown + Duration::minutes(5)),
            RuntimeJobClaimDecision::Proceed
        );
        assert_eq!(
            registry.before_execute(
                &job,
                after_cooldown + Duration::minutes(6),
                after_cooldown + Duration::minutes(11),
            ),
            RuntimeJobClaimDecision::Proceed
        );
        let second_probe = registry.before_execute(
            &second_job,
            after_cooldown + Duration::minutes(6),
            after_cooldown + Duration::minutes(11),
        );
        match second_probe {
            RuntimeJobClaimDecision::Defer { reason, .. } => {
                assert_eq!(reason, "runtime circuit breaker half-open probe in flight");
            }
            RuntimeJobClaimDecision::Proceed => {
                panic!("half-open circuit should allow exactly one probe")
            }
        }
        assert!(registry
            .record_success("codex", &second_job.id, after_cooldown)
            .is_empty());
        assert_eq!(registry.snapshots(after_cooldown)[0].state, "half_open");
        let close_events = registry.record_success("codex", &job.id, after_cooldown);
        assert_eq!(close_events.len(), 1);
        assert_eq!(close_events[0].kind, CircuitBreakerEventKind::Closed);
        assert!(registry.snapshots(after_cooldown).is_empty());
    }

    #[test]
    fn interleaved_failure_classes_count_independently() {
        let registry = test_registry();
        let now = Utc::now();
        registry.record_failure("codex", "job-1", FailureClass::Unclassified, now);
        registry.record_failure("codex", "job-2", FailureClass::CliMissingFile, now);
        registry.record_failure("codex", "job-3", FailureClass::Unclassified, now);

        assert!(registry.defer_open_profiles(now).is_empty());
        assert_eq!(registry.snapshots(now)[0].state, "closed");
    }

    #[test]
    fn reset_closes_profile() {
        let registry = test_registry();
        let now = Utc::now();
        for _ in 0..3 {
            registry.record_failure("codex", "job-1", FailureClass::Unclassified, now);
        }

        let event = registry.reset("codex", now);

        assert_eq!(event.kind, CircuitBreakerEventKind::Reset);
        assert!(registry.snapshots(now).is_empty());
    }

    #[test]
    fn storm_replay_opens_after_fifth_same_class_failure() {
        let registry = RuntimeCircuitBreakerRegistry::new(RuntimeCircuitBreakerPolicy::default());
        let now = Utc::now();

        for _ in 0..4 {
            assert!(registry
                .record_failure(
                    "codex",
                    "storm-job",
                    FailureClass::QuotaInteractiveWait,
                    now
                )
                .is_empty());
            assert!(registry.defer_open_profiles(now).is_empty());
        }
        let events = registry.record_failure(
            "codex",
            "storm-job",
            FailureClass::QuotaInteractiveWait,
            now,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, CircuitBreakerEventKind::Opened);
        assert_eq!(events[0].consecutive, Some(5));
        assert_eq!(registry.defer_open_profiles(now).len(), 1);

        let after_cooldown = now + Duration::seconds(601);
        assert!(registry.defer_open_profiles(after_cooldown).is_empty());
        assert_eq!(registry.snapshots(after_cooldown)[0].state, "half_open");
        let probe = RuntimeJob::pending(
            "command-storm-probe",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex",
            serde_json::json!({ "activity": "check" }),
        );
        assert_eq!(
            registry.before_execute(
                &probe,
                after_cooldown,
                after_cooldown + Duration::minutes(5),
            ),
            RuntimeJobClaimDecision::Proceed
        );
        let reopened = registry.record_failure(
            "codex",
            &probe.id,
            FailureClass::QuotaInteractiveWait,
            after_cooldown,
        );

        assert_eq!(reopened[0].kind, CircuitBreakerEventKind::Opened);
        assert_eq!(
            reopened[0].cooldown_until,
            Some(after_cooldown + Duration::seconds(1200))
        );
        assert_eq!(registry.defer_open_profiles(after_cooldown).len(), 1);
    }
}
