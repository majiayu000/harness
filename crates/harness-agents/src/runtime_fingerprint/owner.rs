//! Independent runtime fingerprint owner lifecycle.

use super::command::PreparedCommand;
use super::environment::SelectedEnvironment;
use super::{
    ConfiguredRuntimeExecutable, ContainmentUnavailableReason, RuntimeFingerprintOptions,
    RuntimeFingerprintProduceError, RUNTIME_FINGERPRINT_OWNER_CAPACITY,
    RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE, RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE,
};
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, AgentStackFingerprintPayload, RuntimeProbeFailureKind,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

static ACTIVE_RUNTIME_FINGERPRINT_OWNERS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static TEST_OWNER_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(super) struct TestOwnerPermit;

#[cfg(test)]
impl TestOwnerPermit {
    pub(super) async fn acquire() -> Self {
        loop {
            if TEST_OWNER_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Self;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
impl Drop for TestOwnerPermit {
    fn drop(&mut self) {
        TEST_OWNER_ACTIVE.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwnerLifecycleEvent {
    ChildRegistered {
        pid: libc::pid_t,
        role: super::RuntimeOwnedChildRole,
    },
    ChildReaped {
        pid: libc::pid_t,
        role: super::RuntimeOwnedChildRole,
    },
    OwnerExited,
}

pub(super) struct OwnerPermit;

impl OwnerPermit {
    pub(super) fn try_acquire() -> Result<Self, RuntimeFingerprintProduceError> {
        try_reserve_owner(&ACTIVE_RUNTIME_FINGERPRINT_OWNERS)?;
        Ok(Self)
    }
}

impl Drop for OwnerPermit {
    fn drop(&mut self) {
        ACTIVE_RUNTIME_FINGERPRINT_OWNERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct CallerCancellation {
    stop_requested: Arc<AtomicBool>,
    armed: bool,
}

impl CallerCancellation {
    fn new(stop_requested: Arc<AtomicBool>) -> Self {
        Self {
            stop_requested,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.stop_requested.store(true, Ordering::Release);
        }
    }
}

pub(super) async fn run(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    selected_environment: SelectedEnvironment,
    prepared_command: PreparedCommand,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    run_inner(
        executable,
        options,
        selected_environment,
        prepared_command,
        None,
    )
    .await
}

#[cfg(test)]
pub(super) async fn run_observed(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    selected_environment: SelectedEnvironment,
    prepared_command: PreparedCommand,
    observer: std::sync::mpsc::Sender<OwnerLifecycleEvent>,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    run_inner(
        executable,
        options,
        selected_environment,
        prepared_command,
        Some(observer),
    )
    .await
}

async fn run_inner(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    selected_environment: SelectedEnvironment,
    prepared_command: PreparedCommand,
    observer: Option<std::sync::mpsc::Sender<OwnerLifecycleEvent>>,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    #[cfg(test)]
    let test_owner_permit = TestOwnerPermit::acquire().await;
    let permit = OwnerPermit::try_acquire()?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let mut cancellation = CallerCancellation::new(Arc::clone(&stop_requested));
    let owner_stop = Arc::clone(&stop_requested);
    let executable = executable.clone();
    let options = options.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("runtime-fingerprint-owner".to_owned())
        .spawn(move || {
            let registry = observer
                .clone()
                .map_or_else(super::registry::OwnerRegistry::new, |observer| {
                    super::registry::OwnerRegistry::with_observer(Some(observer))
                });
            if owner_stop.load(Ordering::Acquire) || ready_tx.send(()).is_err() {
                drop(permit);
                #[cfg(test)]
                drop(test_owner_permit);
                if let Some(observer) = observer {
                    if observer.send(OwnerLifecycleEvent::OwnerExited).is_err() {
                        tracing::debug!("runtime fingerprint owner observer dropped");
                    }
                }
                let _ = exit_tx.send(());
                return;
            }
            let result = super::probe::owner_run(
                &executable,
                &options,
                selected_environment,
                prepared_command,
                &owner_stop,
                &registry,
            );
            let result = match result {
                Ok(envelope) if !registry.is_empty() && !records_retained_cleanup(&envelope) => {
                    Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
                }
                result => result,
            };
            if result_tx.send(result).is_err() {
                tracing::error!("runtime fingerprint caller dropped before owner completion");
            }
            registry.drain_retained();
            drop(permit);
            #[cfg(test)]
            drop(test_owner_permit);
            if let Some(observer) = observer {
                if observer.send(OwnerLifecycleEvent::OwnerExited).is_err() {
                    tracing::debug!("runtime fingerprint owner observer dropped");
                }
            }
            let _ = exit_tx.send(());
        })
        .map_err(|_| {
            RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerStartFailed,
            )
        })?;

    match tokio::time::timeout(RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE, ready_rx).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            cancellation.disarm();
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerStartFailed,
            ));
        }
        Err(_) => {
            stop_requested.store(true, Ordering::Release);
            let joined =
                tokio::time::timeout(RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE, &mut exit_rx)
                    .await
                    .is_ok();
            cancellation.disarm();
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                if joined {
                    ContainmentUnavailableReason::OwnerReadyTimeout
                } else {
                    ContainmentUnavailableReason::OwnerStopJoinTimeout
                },
            ));
        }
    }
    let result = result_rx.await.map_err(|_| {
        RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::OwnerStopJoinTimeout,
        )
    })?;
    cancellation.disarm();
    result
}

fn records_retained_cleanup(envelope: &AgentStackFingerprintEnvelope) -> bool {
    let AgentStackFingerprintPayload::AgentRuntime(payload) = envelope.payload() else {
        return false;
    };
    payload.failures().iter().any(|failure| {
        matches!(
            failure.kind(),
            RuntimeProbeFailureKind::TerminationFailed | RuntimeProbeFailureKind::ReapFailed
        )
    })
}

pub(super) fn try_reserve_owner(
    counter: &AtomicUsize,
) -> Result<(), RuntimeFingerprintProduceError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < RUNTIME_FINGERPRINT_OWNER_CAPACITY).then_some(active + 1)
        })
        .map(|_| ())
        .map_err(|_| {
            RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerCapacityExhausted,
            )
        })
}
