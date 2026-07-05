use super::*;

fn runtime_worker_lease_ttl(
    policy: &harness_core::config::workflow::RuntimeWorkerPolicy,
) -> chrono::Duration {
    let lease_ttl_secs = i64::try_from(policy.lease_ttl_secs.max(1)).unwrap_or(i64::MAX);
    chrono::Duration::seconds(lease_ttl_secs)
}

pub(super) fn runtime_worker_loop_policy(
    policy: harness_core::config::workflow::RuntimeWorkerPolicy,
) -> harness_core::config::workflow::RuntimeWorkerPolicy {
    let mut policy = if policy.enabled {
        policy
    } else {
        harness_core::config::workflow::RuntimeWorkerPolicy::default()
    };
    policy.concurrency = policy.concurrency.max(1);
    policy
}

fn log_runtime_worker_tick_result(
    result: Result<
        anyhow::Result<crate::workflow_runtime_worker::RuntimeJobWorkerTick>,
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok(Ok(tick)) if tick.touched_anything() => {
            tracing::info!(
                succeeded = tick.succeeded,
                failed = tick.failed,
                cancelled = tick.cancelled,
                "workflow runtime job worker tick complete"
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!("workflow runtime job worker tick failed: {e}");
        }
        Err(e) => {
            tracing::warn!("workflow runtime job worker task panicked: {e}");
        }
    }
}

type RuntimeWorkerJoinSet =
    tokio::task::JoinSet<anyhow::Result<crate::workflow_runtime_worker::RuntimeJobWorkerTick>>;

pub(super) fn drain_finished_runtime_worker_ticks(workers: &mut RuntimeWorkerJoinSet) {
    while let Some(result) = workers.try_join_next() {
        log_runtime_worker_tick_result(result);
    }
}

pub(super) fn runtime_worker_open_slots(
    active_workers: usize,
    configured_concurrency: u32,
) -> usize {
    (configured_concurrency as usize).saturating_sub(active_workers)
}

pub(super) fn runtime_worker_has_external_state_owner(
    observed_state_strong_count: usize,
    active_worker_state_clones: usize,
) -> bool {
    observed_state_strong_count > active_worker_state_clones.saturating_add(1)
}

struct RuntimeWorkerStateLease {
    state: Option<Arc<AppState>>,
    active_worker_state_clones: Arc<AtomicUsize>,
}

impl RuntimeWorkerStateLease {
    fn new(state: Arc<AppState>, active_worker_state_clones: Arc<AtomicUsize>) -> Self {
        active_worker_state_clones.fetch_add(1, Ordering::AcqRel);
        Self {
            state: Some(state),
            active_worker_state_clones,
        }
    }

    fn state(&self) -> &Arc<AppState> {
        self.state
            .as_ref()
            .expect("runtime worker state lease must hold app state")
    }
}

impl Drop for RuntimeWorkerStateLease {
    fn drop(&mut self) {
        drop(self.state.take());
        self.active_worker_state_clones
            .fetch_sub(1, Ordering::AcqRel);
    }
}

fn spawn_runtime_worker_ticks(
    workers: &mut RuntimeWorkerJoinSet,
    state: &Arc<AppState>,
    active_worker_state_clones: &Arc<AtomicUsize>,
    concurrency: u32,
    lease_ttl: chrono::Duration,
    next_worker_id: &mut u64,
) {
    let open_slots = runtime_worker_open_slots(workers.len(), concurrency);
    for _ in 0..open_slots {
        let state_lease =
            RuntimeWorkerStateLease::new(state.clone(), active_worker_state_clones.clone());
        let owner = format!("server-runtime-worker-{next_worker_id}");
        *next_worker_id = next_worker_id.saturating_add(1);
        workers.spawn(async move {
            crate::workflow_runtime_worker::run_runtime_job_worker_tick(
                state_lease.state(),
                owner,
                lease_ttl,
            )
            .await
        });
    }
}

fn stop_runtime_job_worker_supervisor_for_shutdown(
    workers: &mut RuntimeWorkerJoinSet,
    shutdown_result: Result<(), tokio::sync::broadcast::error::RecvError>,
) {
    match shutdown_result {
        Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {
            tracing::info!("workflow runtime job worker supervisor stopping for shutdown");
        }
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(
                skipped,
                "workflow runtime job worker supervisor lagged shutdown signal"
            );
        }
    }
    workers.abort_all();
}

pub(super) async fn runtime_worker_sleep_or_shutdown(
    delay: std::time::Duration,
    shutdown_rx: &mut tokio::sync::broadcast::Receiver<()>,
    workers: &mut RuntimeWorkerJoinSet,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        shutdown_result = shutdown_rx.recv() => {
            stop_runtime_job_worker_supervisor_for_shutdown(workers, shutdown_result);
            true
        }
    }
}

pub(in crate::http) fn spawn_runtime_job_workers(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime job workers disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    let mut shutdown_rx = state.notifications.ws_shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut workers = tokio::task::JoinSet::new();
        let active_worker_state_clones = Arc::new(AtomicUsize::new(0));
        let mut next_worker_id = 0;
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = match load_runtime_workflow_config(
                &state.core.project_root,
                "workflow runtime job workers",
            ) {
                Ok(config) => config,
                Err(_) => {
                    drop(state);
                    if runtime_worker_sleep_or_shutdown(
                        std::time::Duration::from_secs(RUNTIME_WORKFLOW_CONFIG_RETRY_SECS),
                        &mut shutdown_rx,
                        &mut workers,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
            };
            let policy = runtime_worker_loop_policy(workflow_cfg.runtime_worker);
            let interval = std::time::Duration::from_secs(policy.interval_secs.max(1));
            let lease_ttl = runtime_worker_lease_ttl(&policy);
            drain_finished_runtime_worker_ticks(&mut workers);
            let worker_state_clones = active_worker_state_clones.load(Ordering::Acquire);
            if !runtime_worker_has_external_state_owner(
                Arc::strong_count(&state),
                worker_state_clones,
            ) {
                tracing::info!(
                    active_workers = workers.len(),
                    worker_state_clones,
                    "workflow runtime job worker supervisor stopping: app state owner dropped"
                );
                workers.abort_all();
                break;
            }
            spawn_runtime_worker_ticks(
                &mut workers,
                &state,
                &active_worker_state_clones,
                policy.concurrency,
                lease_ttl,
                &mut next_worker_id,
            );
            drop(state);
            if runtime_worker_sleep_or_shutdown(interval, &mut shutdown_rx, &mut workers).await {
                break;
            }
        }
    });
}
