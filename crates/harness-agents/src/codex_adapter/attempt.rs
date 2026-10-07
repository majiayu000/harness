//! Attempt-scoped cancellation and private deadline defaults for Codex app-server.
//!
//! See #2095 §3.2–3.3 (PR B). Frame-size bounds live in `bounded_frame` (PR C).

use std::path::PathBuf;
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
    workspace: PathBuf,
    disarmed: Arc<AtomicBool>,
}

impl TurnAttemptGuard {
    pub(super) fn new(
        state: Arc<Mutex<AdapterState>>,
        cancel_notify: Arc<Notify>,
        generation: u64,
        workspace: PathBuf,
    ) -> Self {
        Self {
            state,
            cancel_notify,
            generation,
            workspace,
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
        // Publish ownership before the async cleanup can wait for the state lock.
        // The registry retains the adapter even if the runtime stops before the
        // cleanup task can run. An unacknowledged attempt never authorizes reuse.
        let cleanup = harness_core::process_cleanup::ProcessCleanupAcknowledgement::pending(
            self.workspace.clone(),
        );
        let _ = harness_core::process_cleanup::retain_until_workspace_cleanup(
            &self.workspace,
            state.clone(),
        );
        tracing::warn!(workspace = %self.workspace.display(), attempt_generation = generation,
            "cancelled Codex attempt retains workspace ownership pending process cleanup acknowledgement");
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(workspace = %self.workspace.display(),
                "cancelled Codex attempt retains workspace ownership because no cleanup runtime is available");
            return;
        };
        runtime.spawn(async move {
            loop {
                let mut guard = state.lock().await;
                if !guard.generation_is_current(generation) && guard.failed_cleanup.is_none() {
                    // A concurrent terminate already acknowledged the old attempt.
                    drop(guard);
                    cleanup.confirm();
                    return;
                }
                if let Some(attempt) = guard.active_attempt.as_mut() {
                    attempt.cancel_requested = true;
                    cancel_notify.notify_waiters();
                }
                match guard.reset_child().await {
                    Ok(()) => {
                        guard.clear_attempt_if_current(generation);
                        drop(guard);
                        cleanup.confirm();
                        return;
                    }
                    Err(error) => {
                        tracing::error!(
                            "cancelled Codex attempt cleanup remains unconfirmed: {error}"
                        );
                    }
                }
                drop(guard);
                tokio::time::sleep(Duration::from_secs(1)).await;
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
        // Subscribe before the state check so notify_waiters() between unlock and
        // await cannot be lost (Tokio registers notify_waiters interest on create).
        let notified = cancel_notify.notified();
        {
            let guard = state.lock().await;
            if guard
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| attempt.generation == generation && attempt.cancel_requested)
            {
                return;
            }
            if guard
                .active_attempt
                .as_ref()
                .is_none_or(|attempt| attempt.generation != generation)
            {
                return;
            }
        }
        notified.await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration as TokioDuration};

    #[cfg(unix)]
    #[tokio::test]
    async fn abandoned_attempt_fences_workspace_before_waiting_for_adapter_mutex() {
        struct Owner(Arc<AtomicBool>);
        impl Drop for Owner {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30").kill_on_drop(true);
        crate::set_process_group(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let state = Arc::new(Mutex::new(AdapterState::new()));
        let mut locked = state.lock().await;
        locked.child = Some(
            crate::ManagedChild::new(child, "cancelled-attempt-test")
                .with_cleanup_workspace(workspace.clone()),
        );
        let generation = locked.begin_attempt().unwrap();
        let attempt = TurnAttemptGuard::new(
            state.clone(),
            Arc::new(Notify::new()),
            generation,
            workspace.clone(),
        );
        drop(attempt);
        assert!(harness_core::process_cleanup::workspace_cleanup_pending(
            &workspace
        ));
        let released = Arc::new(AtomicBool::new(false));
        assert!(
            harness_core::process_cleanup::retain_until_workspace_cleanup(
                &workspace,
                Owner(released.clone()),
            )
            .is_ok()
        );
        // The detached cleanup cannot acquire the state mutex yet. Cancellation
        // must already retain its workspace owner while other tasks progress.
        tokio::time::sleep(TokioDuration::from_millis(20)).await;
        assert!(!released.load(Ordering::Acquire));
        assert!(crate::process_group_has_live_members(pid));
        drop(locked);
        timeout(
            TokioDuration::from_secs(3),
            harness_core::process_cleanup::wait_for_workspace_cleanup(&workspace),
        )
        .await
        .unwrap();
        assert!(!crate::process_group_has_live_members(pid));
        assert!(released.load(Ordering::Acquire));
        assert!(state.lock().await.child.is_none());
    }

    #[tokio::test]
    async fn wait_until_cancelled_observes_notify_after_state_check_window() {
        let state = Arc::new(Mutex::new(super::super::AdapterState::new()));
        let cancel_notify = Arc::new(Notify::new());
        let generation = {
            let mut guard = state.lock().await;
            guard
                .begin_attempt()
                .expect("first attempt must allocate a generation")
        };

        let waiter = {
            let state = state.clone();
            let cancel_notify = cancel_notify.clone();
            tokio::spawn(async move {
                wait_until_cancelled(&state, &cancel_notify, generation).await;
            })
        };

        // Yield so the waiter reaches the unlocked notified().await path.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        {
            let mut guard = state.lock().await;
            let attempt = guard
                .active_attempt
                .as_mut()
                .expect("attempt remains active while waiter runs");
            assert_eq!(attempt.generation, generation);
            attempt.cancel_requested = true;
        }
        // notify_waiters after unlock is the race window the subscribe-first fix closes.
        cancel_notify.notify_waiters();

        timeout(TokioDuration::from_millis(500), waiter)
            .await
            .expect("cancel wake must not hang after notify_waiters")
            .expect("waiter task must join");
    }
}
