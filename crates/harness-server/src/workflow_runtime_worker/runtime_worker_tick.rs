use super::executor::ServerRuntimeJobExecutor;
use super::otel_trajectory::emit_runtime_job_trajectory_completion;
use super::*;
use chrono::Duration;
use harness_workflow::runtime::RuntimeWorker;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeJobWorkerTick {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub idle: bool,
}

impl RuntimeJobWorkerTick {
    fn from_completed_job(job: Option<RuntimeJob>) -> Self {
        match job.map(|job| job.status) {
            Some(RuntimeJobStatus::Succeeded) => Self {
                succeeded: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Failed) => Self {
                failed: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Cancelled) => Self {
                cancelled: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Pending | RuntimeJobStatus::Running) => Self::default(),
            None => Self {
                idle: true,
                ..Self::default()
            },
        }
    }

    pub(crate) fn touched_anything(&self) -> bool {
        self.succeeded > 0 || self.failed > 0 || self.cancelled > 0
    }
}

#[cfg(test)]
pub(crate) async fn run_runtime_job_worker_tick(
    state: &Arc<AppState>,
    owner: impl Into<String>,
    lease_ttl: Duration,
) -> anyhow::Result<RuntimeJobWorkerTick> {
    run_runtime_job_worker_tick_inner(state, owner, lease_ttl, None).await
}

pub(crate) async fn run_runtime_job_worker_tick_until_shutdown(
    state: &Arc<AppState>,
    owner: impl Into<String>,
    lease_ttl: Duration,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<RuntimeJobWorkerTick> {
    run_runtime_job_worker_tick_inner(state, owner, lease_ttl, Some(&mut shutdown)).await
}

async fn run_runtime_job_worker_tick_inner(
    state: &Arc<AppState>,
    owner: impl Into<String>,
    lease_ttl: Duration,
    shutdown: Option<&mut tokio::sync::broadcast::Receiver<()>>,
) -> anyhow::Result<RuntimeJobWorkerTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeJobWorkerTick {
            idle: true,
            ..RuntimeJobWorkerTick::default()
        });
    };
    defer_open_runtime_profiles(state, store.as_ref()).await?;
    let worker = RuntimeWorker::new(store.as_ref(), owner)
        .with_lease_ttl(lease_ttl)
        .with_claim_guard(state.runtime_circuit_breakers.as_ref());
    let executor = ServerRuntimeJobExecutor::new(state);
    let execution = worker.run_once(&executor);
    tokio::pin!(execution);
    let mut completed = match shutdown {
        Some(shutdown) => {
            tokio::select! {
                biased;
                result = &mut execution => result?,
                _ = shutdown.recv() => {
                    executor.cancel_lease_lost();
                    execution.await?
                }
            }
        }
        None => execution.await?,
    };
    if let Some(job) = completed.as_mut() {
        if let Err(error) = emit_runtime_job_trajectory_completion(state, store.as_ref(), job).await
        {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime OTel trajectory emission failed: {error}"
            );
        }
        record_runtime_circuit_breaker_completion(state, store.as_ref(), job).await?;
        if let Err(error) = notify_runtime_submission_terminal(state, job).await {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime completion notification failed: {error}"
            );
        }
    }
    Ok(RuntimeJobWorkerTick::from_completed_job(completed))
}
