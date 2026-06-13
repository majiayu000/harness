use harness_core::config::agents::ClaudeProviderBackpressureConfig;
use harness_core::error::HarnessError;
use harness_core::types::ExecutionPhase;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderPhase {
    Triage,
    Planning,
    Execution,
    Validation,
    Rebase,
    SimpleReview,
    Unknown,
}

impl ProviderPhase {
    pub fn from_execution_phase(phase: Option<ExecutionPhase>) -> Self {
        match phase {
            Some(ExecutionPhase::Triage) => Self::Triage,
            Some(ExecutionPhase::Planning) => Self::Planning,
            Some(ExecutionPhase::Execution) => Self::Execution,
            Some(ExecutionPhase::Validation) => Self::Validation,
            Some(ExecutionPhase::Rebase) => Self::Rebase,
            Some(ExecutionPhase::SimpleReview) => Self::SimpleReview,
            None => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Planning => "planning",
            Self::Execution => "execution",
            Self::Validation => "validation",
            Self::Rebase => "rebase",
            Self::SimpleReview => "simple_review",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderBackpressureGate {
    global: Option<Arc<Semaphore>>,
    phase_limits: HashMap<ProviderPhase, Arc<Semaphore>>,
}

#[derive(Debug)]
pub struct ProviderBackpressurePermit {
    phase: ProviderPhase,
    waited_ms: u64,
    _phase_permit: Option<OwnedSemaphorePermit>,
    _global_permit: Option<OwnedSemaphorePermit>,
}

impl ProviderBackpressurePermit {
    pub fn phase(&self) -> ProviderPhase {
        self.phase
    }

    pub fn waited_ms(&self) -> u64 {
        self.waited_ms
    }
}

impl ProviderBackpressureGate {
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn from_claude_config(config: &ClaudeProviderBackpressureConfig) -> Self {
        let global = config
            .max_concurrent_sessions
            .map(|limit| Arc::new(Semaphore::new(limit.get())));
        let mut phase_limits = HashMap::new();
        for (phase, limit) in phase_limit_entries(config) {
            phase_limits.insert(phase, Arc::new(Semaphore::new(limit.get())));
        }
        Self {
            global,
            phase_limits,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.global.is_some() || !self.phase_limits.is_empty()
    }

    pub async fn acquire(
        &self,
        phase: Option<ExecutionPhase>,
        prompt_chars: usize,
        prompt_bytes: usize,
    ) -> harness_core::error::Result<ProviderBackpressurePermit> {
        let phase = ProviderPhase::from_execution_phase(phase);
        if !self.is_enabled() {
            return Ok(ProviderBackpressurePermit {
                phase,
                waited_ms: 0,
                _phase_permit: None,
                _global_permit: None,
            });
        }

        let started = Instant::now();
        tracing::info!(
            provider = "claude",
            phase = phase.label(),
            prompt_chars,
            prompt_bytes,
            "waiting for provider capacity"
        );

        let phase_permit = if let Some(semaphore) = self.phase_limits.get(&phase) {
            Some(acquire_owned(semaphore.clone(), phase.label()).await?)
        } else {
            None
        };
        let global_permit = if let Some(semaphore) = &self.global {
            Some(acquire_owned(semaphore.clone(), "global").await?)
        } else {
            None
        };
        let waited_ms = started.elapsed().as_millis() as u64;
        tracing::info!(
            provider = "claude",
            phase = phase.label(),
            waited_ms,
            prompt_chars,
            prompt_bytes,
            "provider capacity admitted"
        );

        Ok(ProviderBackpressurePermit {
            phase,
            waited_ms,
            _phase_permit: phase_permit,
            _global_permit: global_permit,
        })
    }
}

async fn acquire_owned(
    semaphore: Arc<Semaphore>,
    scope: &'static str,
) -> harness_core::error::Result<OwnedSemaphorePermit> {
    semaphore.acquire_owned().await.map_err(|error| {
        HarnessError::AgentExecution(format!(
            "claude provider capacity gate closed while waiting for {scope} permit: {error}"
        ))
    })
}

fn phase_limit_entries(
    config: &ClaudeProviderBackpressureConfig,
) -> impl Iterator<Item = (ProviderPhase, NonZeroUsize)> + '_ {
    [
        (ProviderPhase::Triage, config.phase_limits.triage),
        (ProviderPhase::Planning, config.phase_limits.planning),
        (ProviderPhase::Execution, config.phase_limits.execution),
        (ProviderPhase::Validation, config.phase_limits.validation),
        (ProviderPhase::Rebase, config.phase_limits.rebase),
        (
            ProviderPhase::SimpleReview,
            config.phase_limits.simple_review,
        ),
        (ProviderPhase::Unknown, config.phase_limits.unknown),
    ]
    .into_iter()
    .filter_map(|(phase, limit)| limit.map(|limit| (phase, limit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::agents::{
        ClaudeProviderBackpressureConfig, ClaudeProviderPhaseLimits,
    };
    use std::num::NonZeroUsize;
    use std::time::Duration;

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test limit must be non-zero")
    }

    #[tokio::test]
    async fn global_limit_queues_second_claude_session() {
        let config = ClaudeProviderBackpressureConfig {
            max_concurrent_sessions: Some(nonzero(1)),
            phase_limits: ClaudeProviderPhaseLimits::default(),
        };
        let gate = Arc::new(ProviderBackpressureGate::from_claude_config(&config));
        let first = gate
            .acquire(Some(ExecutionPhase::Execution), 10, 10)
            .await
            .expect("first permit");

        let waiter_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            waiter_gate
                .acquire(Some(ExecutionPhase::Execution), 20, 20)
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "second Claude session should queue at global provider cap"
        );

        drop(first);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should unblock")
            .expect("waiter task should join")
            .expect("second permit should acquire");
    }

    #[tokio::test]
    async fn phase_limit_blocks_same_phase_without_blocking_execution_phase() {
        let config = ClaudeProviderBackpressureConfig {
            max_concurrent_sessions: Some(nonzero(2)),
            phase_limits: ClaudeProviderPhaseLimits {
                planning: Some(nonzero(1)),
                ..ClaudeProviderPhaseLimits::default()
            },
        };
        let gate = Arc::new(ProviderBackpressureGate::from_claude_config(&config));
        let planning = gate
            .acquire(Some(ExecutionPhase::Planning), 10, 10)
            .await
            .expect("planning permit");

        let planning_waiter_gate = gate.clone();
        let planning_waiter = tokio::spawn(async move {
            planning_waiter_gate
                .acquire(Some(ExecutionPhase::Planning), 20, 20)
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !planning_waiter.is_finished(),
            "same phase should queue behind the phase cap"
        );

        let execution = tokio::time::timeout(
            Duration::from_secs(1),
            gate.acquire(Some(ExecutionPhase::Execution), 30, 30),
        )
        .await
        .expect("execution phase should not wait for planning cap")
        .expect("execution permit should acquire");
        assert_eq!(execution.phase(), ProviderPhase::Execution);
        drop(execution);

        drop(planning);
        tokio::time::timeout(Duration::from_secs(1), planning_waiter)
            .await
            .expect("planning waiter should unblock")
            .expect("planning waiter task should join")
            .expect("planning waiter should acquire");
    }
}
