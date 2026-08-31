//! Stream execution and accounting for one pinned agent-contract attempt.

use harness_core::agent::{AgentBackend, AgentEvent, AgentRequest, AGENT_OUTPUT_SCHEMA_PATH_ENV};
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::config::workflow::agent_contract_output_schema_document;
use harness_core::types::TurnId;
use std::sync::Arc;

use super::agent_contract_attempt::ContractAttempt;
use super::agent_contract_enforcement::{
    ensure_backend_can_enforce_contract, PinnedJobAgentContract, TurnStreamObservations,
};
use super::agent_contract_prompt::contract_attempt_prompt;
use super::turn_engine::helpers::{RuntimeUsageContext, TurnBudgetStop};

#[derive(Debug)]
pub(super) struct ContractAttemptFailure {
    pub(super) attempt: ContractAttempt,
    source: anyhow::Error,
}

impl ContractAttemptFailure {
    fn new(attempt: ContractAttempt, source: anyhow::Error) -> Self {
        Self { attempt, source }
    }

    pub(super) fn into_parts(self) -> (ContractAttempt, anyhow::Error) {
        (self.attempt, self.source)
    }
}

impl std::fmt::Display for ContractAttemptFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ContractAttemptFailure {}

pub(super) async fn execute_agent_contract_attempt(
    backend: Arc<dyn AgentBackend>,
    pinned: &PinnedJobAgentContract,
    model: Option<String>,
    reasoning_effort: Option<String>,
    timeout_secs: u64,
    correction: Option<(&str, &str)>,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
    mut lease_lost: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<ContractAttempt> {
    if timeout_secs == 0 {
        anyhow::bail!("agent contract attempt timeout_secs must be positive");
    }
    ensure_backend_can_enforce_contract(backend.as_ref())?;
    let schema_document = agent_contract_output_schema_document(&pinned.contract.output_schema)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "output schema `{}` has no canonical schema document to enforce",
                pinned.contract.output_schema
            )
        })?;
    let base_prompt = contract_attempt_prompt(pinned)?;
    let prompt = match correction {
        Some((previous_output, validation_error)) => format!(
            "{base_prompt}\n\nThe previous reply failed server validation. Return a corrected verdict using the same pinned facts and no tools.\n\nValidation error:\n{validation_error}\n\nPrevious reply:\n{previous_output}"
        ),
        None => base_prompt,
    };

    let workspace = tempfile::Builder::new()
        .prefix("harness-agent-contract-workspace-")
        .tempdir()?;
    let schema_dir = tempfile::Builder::new()
        .prefix("harness-agent-contract-schema-")
        .tempdir()?;
    let schema_path = schema_dir.path().join("output-schema.json");
    std::fs::write(&schema_path, schema_document)?;

    let request = AgentRequest {
        prompt,
        project_root: workspace.path().to_path_buf(),
        permission_mode: AgentPermissionMode::Full,
        allowed_tools: Some(Vec::new()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        approval_policy: Some("never".to_string()),
        model,
        reasoning_effort,
        timeout_secs: Some(timeout_secs),
        env_vars: std::iter::once((
            AGENT_OUTPUT_SCHEMA_PATH_ENV.to_string(),
            schema_path.display().to_string(),
        ))
        .collect(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let stream_backend = Arc::clone(&backend);
    let mut stream = tokio::spawn(async move { stream_backend.execute_stream(request, tx).await });
    let mut attempt = ContractAttempt {
        output: String::new(),
        items: Vec::new(),
        observations: TurnStreamObservations::default(),
    };
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    let mut event_channel_open = true;
    let stream_result = loop {
        tokio::select! {
            biased;
            _ = async {
                let Some(receiver) = lease_lost.as_mut() else {
                    std::future::pending::<()>().await;
                    return;
                };
                while !*receiver.borrow() {
                    if receiver.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            } => {
                let error = anyhow::anyhow!(
                    "agent contract attempt cancelled after runtime lease loss"
                );
                let error = stop_and_drain(
                    &mut stream,
                    &mut rx,
                    &mut attempt,
                    runtime_usage,
                    "lease-loss",
                    error,
                )
                .await;
                return Err(ContractAttemptFailure::new(attempt, error).into());
            }
            _ = &mut deadline => {
                let error = anyhow::anyhow!(
                    "agent contract attempt timed out after {timeout_secs}s"
                );
                let error = stop_and_drain(
                    &mut stream,
                    &mut rx,
                    &mut attempt,
                    runtime_usage,
                    "timeout",
                    error,
                )
                .await;
                return Err(ContractAttemptFailure::new(attempt, error).into());
            }
            event = rx.recv(), if event_channel_open => {
                match event {
                    Some(event) => match record_event(&mut attempt, event, runtime_usage).await {
                        Ok(Some(stop)) => {
                            let error = stop_and_drain(
                                &mut stream,
                                &mut rx,
                                &mut attempt,
                                runtime_usage,
                                "budget-stop",
                                budget_stop_error(&stop),
                            )
                            .await;
                            return Err(ContractAttemptFailure::new(attempt, error).into());
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let error = stop_and_drain(
                                &mut stream,
                                &mut rx,
                                &mut attempt,
                                runtime_usage,
                                "usage-accounting",
                                error,
                            )
                            .await;
                            return Err(ContractAttemptFailure::new(attempt, error).into());
                        }
                    },
                    None => event_channel_open = false,
                }
            }
            result = &mut stream => break result,
        }
    };

    match drain_events(&mut rx, &mut attempt, runtime_usage).await {
        Ok(Some(stop)) => {
            return Err(ContractAttemptFailure::new(attempt, budget_stop_error(&stop)).into())
        }
        Ok(None) => {}
        Err(error) => return Err(ContractAttemptFailure::new(attempt, error).into()),
    }
    if let Err(error) = stream_result
        .map_err(|error| anyhow::anyhow!("contract attempt stream task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("contract attempt launch failed: {error}"))
    {
        return Err(ContractAttemptFailure::new(attempt, error).into());
    }
    Ok(attempt)
}

async fn drain_events(
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    attempt: &mut ContractAttempt,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
) -> anyhow::Result<Option<TurnBudgetStop>> {
    let mut budget_stop = None;
    while let Ok(event) = rx.try_recv() {
        if let Some(stop) = record_event(attempt, event, runtime_usage).await? {
            budget_stop.get_or_insert(stop);
        }
    }
    Ok(budget_stop)
}

async fn stop_and_drain(
    stream: &mut tokio::task::JoinHandle<harness_core::error::Result<()>>,
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    attempt: &mut ContractAttempt,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
    reason: &str,
    primary: anyhow::Error,
) -> anyhow::Error {
    let cleanup_error = cancel_stream(stream, reason).await.err();
    let drain_error = drain_events(rx, attempt, runtime_usage).await.err();
    match (cleanup_error, drain_error) {
        (None, None) => primary,
        (Some(cleanup), None) => anyhow::anyhow!("{primary}; cleanup failed: {cleanup}"),
        (None, Some(drain)) => anyhow::anyhow!("{primary}; event drain failed: {drain}"),
        (Some(cleanup), Some(drain)) => {
            anyhow::anyhow!("{primary}; cleanup failed: {cleanup}; event drain failed: {drain}")
        }
    }
}

async fn record_event(
    attempt: &mut ContractAttempt,
    event: AgentEvent,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
) -> anyhow::Result<Option<TurnBudgetStop>> {
    let usage = match &event {
        AgentEvent::TokenUsage { usage } => Some(usage.clone()),
        _ => None,
    };
    attempt.record_event(event);
    let (Some(usage), Some((context, turn_id))) = (usage, runtime_usage) else {
        return Ok(None);
    };
    context.persist_token_usage(turn_id, &usage).await?;
    context.budget_stop().await
}

async fn cancel_stream(
    stream: &mut tokio::task::JoinHandle<harness_core::error::Result<()>>,
    reason: &str,
) -> anyhow::Result<()> {
    stream.abort();
    match stream.await {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => anyhow::bail!("agent contract attempt {reason} cleanup failed: {error}"),
        Ok(Err(error)) => {
            anyhow::bail!("agent contract attempt stream failed during {reason} cleanup: {error}")
        }
        Ok(Ok(())) => Ok(()),
    }
}

fn budget_stop_error(stop: &TurnBudgetStop) -> anyhow::Error {
    anyhow::anyhow!(
        "Workflow {} spent {:.2} USD, reaching its {:.2} USD budget; agent contract attempt stopped.",
        stop.workflow_id,
        stop.spent_usd,
        stop.budget_usd
    )
}
