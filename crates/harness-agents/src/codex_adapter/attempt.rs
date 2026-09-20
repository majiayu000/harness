//! Attempt-scoped cancellation and private deadline defaults for Codex app-server.
//!
//! See #2095 §3.2–3.3 (PR B). Frame-size bounds remain PR C.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

use super::AdapterState;

/// Absolute handshake window spanning initialize + initialized + thread/start.
///
/// Chosen above typical cold app-server bring-up while remaining tight enough
/// that a flood of unrelated notifications cannot stall control indefinitely.
/// Distinct from per-message stdout-stall timeout (`AgentRequest::timeout_secs`).
pub(super) const DEFAULT_ABSOLUTE_INIT_DEADLINE: Duration = Duration::from_secs(120);

/// Complete stdin JSON frame write + flush bound.
///
/// Large enough for normal protocol frames; short enough that a child which
/// stops reading stdin cannot block stop/cancel behind an unbounded pipe write.
pub(super) const DEFAULT_FRAME_WRITE_DEADLINE: Duration = Duration::from_secs(30);

/// Stop/cleanup bound for terminate + descendant reap.
///
/// Enforced even when execution/stdout-stall timeout is disabled. Timeout yields
/// cleanup-failure (ownership retained), never a drained success.
pub(super) const DEFAULT_STOP_CLEANUP_DEADLINE: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy)]
pub(crate) struct AttemptDeadlines {
    pub absolute_init: Duration,
    pub frame_write: Duration,
    pub stop_cleanup: Duration,
}

impl Default for AttemptDeadlines {
    fn default() -> Self {
        Self {
            absolute_init: DEFAULT_ABSOLUTE_INIT_DEADLINE,
            frame_write: DEFAULT_FRAME_WRITE_DEADLINE,
            stop_cleanup: DEFAULT_STOP_CLEANUP_DEADLINE,
        }
    }
}

#[derive(Debug)]
pub(super) struct ActiveAttempt {
    pub generation: u64,
    pub cancel_requested: bool,
    pub remote_turn_id: Option<String>,
}

/// RAII owner for a local turn attempt. Dropping an unfinished `start_turn`
/// marks cancel and schedules cleanup so ManagedChild ownership is not orphaned.
pub(super) struct TurnAttemptGuard {
    state: Arc<Mutex<AdapterState>>,
    cancel_notify: Arc<Notify>,
    generation: u64,
    disarmed: Arc<AtomicBool>,
}

impl TurnAttemptGuard {
    pub(super) fn new(
        state: Arc<Mutex<AdapterState>>,
        cancel_notify: Arc<Notify>,
        generation: u64,
    ) -> Self {
        Self {
            state,
            cancel_notify,
            generation,
            disarmed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn disarm(&self) {
        self.disarmed.store(true, Ordering::Release);
    }
}

impl Drop for TurnAttemptGuard {
    fn drop(&mut self) {
        if self.disarmed.load(Ordering::Acquire) {
            return;
        }
        let state = self.state.clone();
        let cancel_notify = self.cancel_notify.clone();
        let generation = self.generation;
        // Best-effort cleanup owner when the start_turn future is cancelled.
        // Concurrent terminate_and_drain remains serialized on the same mutex and
        // preserves failed-cleanup reuse gating from PR A.
        tokio::spawn(async move {
            let mut guard = state.lock().await;
            let Some(attempt) = guard.active_attempt.as_mut() else {
                return;
            };
            if attempt.generation != generation {
                return;
            }
            attempt.cancel_requested = true;
            cancel_notify.notify_waiters();
            let _ = guard.reset_child().await;
            if guard
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.generation == generation)
            {
                guard.active_attempt = None;
            }
        });
    }
}

pub(super) async fn wait_until_cancelled(
    state: &Arc<Mutex<AdapterState>>,
    cancel_notify: &Notify,
    generation: u64,
) {
    loop {
        {
            let guard = state.lock().await;
            if guard
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.generation == generation && attempt.cancel_requested)
            {
                return;
            }
            if !guard
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.generation == generation)
            {
                return;
            }
        }
        cancel_notify.notified().await;
    }
}

pub(super) fn cancelled_error() -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(
        "codex turn attempt cancelled before completion".into(),
    )
}

pub(super) fn overlapping_start_error() -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(
        "codex adapter rejected overlapping start_turn on the same instance".into(),
    )
}

pub(super) fn stale_generation_error() -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(
        "codex adapter attempt generation is no longer current".into(),
    )
}

pub(super) fn absolute_init_deadline_error(budget: Duration) -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(format!(
        "codex app-server absolute initialize deadline ({budget:?}) elapsed before handshake completed"
    ))
}

pub(super) fn frame_write_deadline_error(budget: Duration) -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(format!(
        "codex app-server frame write deadline ({budget:?}) elapsed"
    ))
}

pub(super) fn stop_cleanup_deadline_error(budget: Duration) -> harness_core::error::HarnessError {
    harness_core::error::HarnessError::AgentExecution(format!(
        "codex app-server stop/cleanup deadline ({budget:?}) elapsed before descendant cleanup confirmed"
    ))
}
