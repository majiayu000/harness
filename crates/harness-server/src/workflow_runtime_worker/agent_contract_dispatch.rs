//! Job-level dispatch for pinned agent contracts.

use crate::http::AppState;
use harness_core::config::workflow::agent_contract_output_schema_document;
use harness_workflow::runtime::{ActivityErrorKind, ActivityResult, RuntimeJob};
use std::sync::Arc;
use std::{error::Error, fmt};

use super::agent_contract_enforcement::{
    ensure_backend_can_enforce_contract, PinnedJobAgentContract,
};
use super::agent_contract_execution::execute_contract_attempts;
use super::data_helpers::activity_name;
use super::job_context::{project_root_for_job, workflow_for_job};
use super::runtime_profile::{agent_backend_for_runtime_kind, runtime_profile_for_job};
use super::runtime_usage::runtime_usage_context;

#[derive(Debug)]
pub(super) struct AgentContractExecutionError {
    source: anyhow::Error,
}

impl AgentContractExecutionError {
    pub(super) fn new(source: anyhow::Error) -> Self {
        Self { source }
    }
}

impl fmt::Display for AgentContractExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for AgentContractExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.source()
    }
}

pub(super) fn pinned_agent_contract_for_execution(
    job: &RuntimeJob,
) -> anyhow::Result<Option<PinnedJobAgentContract>> {
    super::agent_contract_enforcement::pinned_agent_contract_for_job(job)
        .map_err(|error| AgentContractExecutionError::new(error).into())
}

pub(super) async fn execute_contract_job(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    pinned: PinnedJobAgentContract,
    lease_lost: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<ActivityResult> {
    execute_contract_job_inner(state, job, pinned, lease_lost)
        .await
        .map_err(|error| AgentContractExecutionError::new(error).into())
}

async fn execute_contract_job_inner(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    pinned: PinnedJobAgentContract,
    lease_lost: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<ActivityResult> {
    let activity = activity_name(job);
    let backend =
        agent_backend_for_runtime_kind(&state.core.server.agent_registry, job.runtime_kind)?;
    if let Err(error) = ensure_backend_can_enforce_contract(backend.as_ref()) {
        return Ok(contract_preflight_failure(
            job,
            &activity,
            &error.to_string(),
        ));
    }
    if agent_contract_output_schema_document(&pinned.contract.output_schema).is_none() {
        return Ok(contract_preflight_failure(
            job,
            &activity,
            &format!(
                "output schema `{}` has no canonical schema document to enforce",
                pinned.contract.output_schema
            ),
        ));
    }
    let profile = runtime_profile_for_job(job)?;
    let Some(timeout_secs) = profile.timeout_secs.filter(|timeout| *timeout > 0) else {
        return Ok(contract_preflight_failure(
            job,
            &activity,
            "the pinned runtime profile has no positive timeout_secs",
        ));
    };
    let workflow = workflow_for_job(state, job).await?;
    let source_project_root = project_root_for_job(state, job, workflow.as_ref())?;
    let runtime_usage = runtime_usage_context(
        state,
        job,
        workflow.as_ref(),
        &profile,
        backend.name(),
        backend.reports_usage_cost(),
        &source_project_root,
    );
    execute_contract_attempts(
        state,
        job,
        backend,
        &pinned,
        &activity,
        profile.model.clone(),
        profile.reasoning_effort.clone(),
        timeout_secs,
        profile.max_turns,
        runtime_usage.as_ref(),
        lease_lost,
    )
    .await
}

fn contract_preflight_failure(job: &RuntimeJob, activity: &str, reason: &str) -> ActivityResult {
    ActivityResult::failed(
        activity,
        format!(
            "Runtime job {} carries a pinned agent_contract that cannot be enforced.",
            job.id
        ),
        reason,
    )
    .with_error_kind(ActivityErrorKind::Fatal)
}
