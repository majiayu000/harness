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
pub(super) use super::turn_engine::runtime_usage::{
    budget_stop_artifact, enforced_budget_cost_error,
};

#[derive(Debug)]
pub(super) struct ContractAttemptFailure {
    pub(super) attempt: ContractAttempt,
    pub(super) budget_stop: Option<TurnBudgetStop>,
    source: anyhow::Error,
}

impl ContractAttemptFailure {
    fn new(attempt: ContractAttempt, source: anyhow::Error) -> Self {
        Self {
            attempt,
            budget_stop: None,
            source,
        }
    }

    fn budget(attempt: ContractAttempt, stop: TurnBudgetStop, source: anyhow::Error) -> Self {
        Self {
            attempt,
            budget_stop: Some(stop),
            source,
        }
    }

    pub(super) fn into_parts(self) -> (ContractAttempt, anyhow::Error, Option<TurnBudgetStop>) {
        (self.attempt, self.source, self.budget_stop)
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
                let (error, budget_stop) = stop_and_drain(
                    &mut stream,
                    &mut rx,
                    &mut attempt,
                    runtime_usage,
                    "lease-loss",
                    error,
                )
                .await;
                let mut failure = ContractAttemptFailure::new(attempt, error);
                failure.budget_stop = budget_stop;
                return Err(failure.into());
            }
            _ = &mut deadline => {
                let error = anyhow::anyhow!(
                    "agent contract attempt timed out after {timeout_secs}s"
                );
                let (error, budget_stop) = stop_and_drain(
                    &mut stream,
                    &mut rx,
                    &mut attempt,
                    runtime_usage,
                    "timeout",
                    error,
                )
                .await;
                let mut failure = ContractAttemptFailure::new(attempt, error);
                failure.budget_stop = budget_stop;
                return Err(failure.into());
            }
            event = rx.recv(), if event_channel_open => {
                match event {
                    Some(event) => match record_event(&mut attempt, event, runtime_usage).await {
                        Ok(Some(stop)) => {
                            let (error, _) = stop_and_drain(
                                &mut stream,
                                &mut rx,
                                &mut attempt,
                                runtime_usage,
                                "budget-stop",
                                budget_stop_error(&stop),
                            )
                            .await;
                            return Err(ContractAttemptFailure::budget(attempt, stop, error).into());
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let (error, budget_stop) = stop_and_drain(
                                &mut stream,
                                &mut rx,
                                &mut attempt,
                                runtime_usage,
                                "usage-accounting",
                                error,
                            )
                            .await;
                            let mut failure = ContractAttemptFailure::new(attempt, error);
                            failure.budget_stop = budget_stop;
                            return Err(failure.into());
                        }
                    },
                    None => event_channel_open = false,
                }
            }
            result = &mut stream => break result,
        }
    };

    let (budget_stop, drain_error) = drain_events(&mut rx, &mut attempt, runtime_usage).await;
    if let Some(stop) = budget_stop {
        let error = budget_stop_error(&stop);
        return Err(ContractAttemptFailure::budget(attempt, stop, error).into());
    }
    if let Some(error) = drain_error {
        return Err(ContractAttemptFailure::new(attempt, error).into());
    }
    if let Err(error) = stream_result
        .map_err(|error| anyhow::anyhow!("contract attempt stream task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("contract attempt launch failed: {error}"))
    {
        return Err(ContractAttemptFailure::new(attempt, error).into());
    }
    if runtime_usage.is_some_and(|(context, _)| {
        !context.budget_policy.unlimited
            && context.budget_policy.enforcement
                == harness_core::config::workflow::RuntimeBudgetEnforcement::Enforce
    }) && !attempt.observations.cost_usd_observed
    {
        let error = anyhow::anyhow!(
            "agent backend did not emit observable USD cost usage under enforced workflow budget policy"
        );
        return Err(ContractAttemptFailure::new(attempt, error).into());
    }
    Ok(attempt)
}

async fn drain_events(
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    attempt: &mut ContractAttempt,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
) -> (Option<TurnBudgetStop>, Option<anyhow::Error>) {
    let mut budget_stop = None;
    while let Ok(event) = rx.try_recv() {
        match record_event(attempt, event, runtime_usage).await {
            Ok(Some(stop)) => {
                budget_stop.get_or_insert(stop);
            }
            Ok(None) => {}
            Err(error) => return (budget_stop, Some(error)),
        }
    }
    (budget_stop, None)
}

async fn stop_and_drain(
    stream: &mut tokio::task::JoinHandle<harness_core::error::Result<()>>,
    rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    attempt: &mut ContractAttempt,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
    reason: &str,
    primary: anyhow::Error,
) -> (anyhow::Error, Option<TurnBudgetStop>) {
    let cleanup_error = cancel_stream(stream, reason).await.err();
    let (budget_stop, drain_error) = drain_events(rx, attempt, runtime_usage).await;
    let error = match (cleanup_error, drain_error) {
        (None, None) => primary,
        (Some(cleanup), None) => anyhow::anyhow!("{primary}; cleanup failed: {cleanup}"),
        (None, Some(drain)) => anyhow::anyhow!("{primary}; event drain failed: {drain}"),
        (Some(cleanup), Some(drain)) => {
            anyhow::anyhow!("{primary}; cleanup failed: {cleanup}; event drain failed: {drain}")
        }
    };
    (error, budget_stop)
}

async fn record_event(
    attempt: &mut ContractAttempt,
    event: AgentEvent,
    runtime_usage: Option<(&RuntimeUsageContext, &TurnId)>,
) -> anyhow::Result<Option<TurnBudgetStop>> {
    let usage = match &event {
        AgentEvent::TokenUsage {
            usage,
            cost_usd_observed,
        } => Some((usage.clone(), *cost_usd_observed)),
        _ => None,
    };
    attempt.record_event(event);
    let (Some((usage, cost_usd_observed)), Some((context, turn_id))) = (usage, runtime_usage)
    else {
        return Ok(None);
    };
    context
        .persist_token_usage(turn_id, &usage, cost_usd_observed)
        .await?;
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

#[cfg(test)]
mod accounting_tests {
    use super::*;
    use crate::workflow_runtime_worker::turn_engine::helpers::RuntimeUsageContext;
    use async_trait::async_trait;
    use harness_core::agent::{AgentBackend, AgentContractCapabilities, AgentEvent, AgentRequest};
    use harness_core::config::workflow::{
        RuntimeBudgetEnforcement, RuntimeBudgetPolicy, WorkflowAgentContract,
    };
    use harness_core::types::TurnId;
    use harness_workflow::runtime::{RuntimeKind, WorkflowRuntimeStore};
    use serde_json::json;
    use std::sync::Arc;

    struct AccountingAgent {
        emit_usage: bool,
        fail_after_usage: bool,
        cost_usd_observed: bool,
    }

    #[async_trait]
    impl AgentBackend for AccountingAgent {
        fn name(&self) -> &str {
            "accounting-agent"
        }

        fn agent_contract_capabilities(&self) -> AgentContractCapabilities {
            AgentContractCapabilities {
                prompt_only_launch: true,
                pinned_output_schema: true,
                attempt_observation_stream: true,
            }
        }

        fn reports_usage_cost(&self) -> bool {
            true
        }

        async fn execute_stream(
            &self,
            _request: AgentRequest,
            tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> harness_core::error::Result<()> {
            if self.emit_usage {
                tx.send(AgentEvent::TokenUsage {
                    usage: harness_core::types::TokenUsage {
                        input_tokens: 11,
                        output_tokens: 7,
                        total_tokens: 18,
                        cost_usd: 0.125,
                    },
                    cost_usd_observed: self.cost_usd_observed,
                })
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
            }
            if self.fail_after_usage {
                tx.send(AgentEvent::ModelReported {
                    model: "gpt-test".to_string(),
                    source: harness_core::agent::ModelIdentitySource::LaunchDerived,
                })
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
                tx.send(AgentEvent::ToolCall {
                    name: "forbidden-tool".to_string(),
                    input: json!({}),
                })
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
                tx.send(AgentEvent::ApprovalRequest {
                    id: "approval-1".to_string(),
                    command: "forbidden command".to_string(),
                })
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
                return Err(harness_core::error::HarnessError::AgentExecution(
                    "backend failed after reporting usage".to_string(),
                ));
            }
            tx.send(AgentEvent::TurnCompleted {
                output: json!({
                    "schema": "harness.semantic_verdict.v1",
                    "outcome": "small",
                    "rationale": "The supplied facts describe one bounded change.",
                    "evidence_refs": []
                })
                .to_string(),
            })
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(error.to_string())
            })?;
            Ok(())
        }
    }

    struct LeaseLossAfterUsageAgent {
        lease_lost: tokio::sync::watch::Sender<bool>,
    }

    #[test]
    fn enforced_budget_requires_backend_reported_cost() {
        struct CostBlindAgent;
        #[async_trait]
        impl AgentBackend for CostBlindAgent {
            fn name(&self) -> &str {
                "cost-blind-agent"
            }
        }

        let enforce = RuntimeBudgetPolicy {
            enforcement: RuntimeBudgetEnforcement::Enforce,
            ..Default::default()
        };
        let shadow = RuntimeBudgetPolicy::default();
        assert!(enforced_budget_cost_error(&CostBlindAgent, &enforce).is_some());
        assert!(enforced_budget_cost_error(&CostBlindAgent, &shadow).is_none());
        assert!(enforced_budget_cost_error(
            &AccountingAgent {
                emit_usage: true,
                fail_after_usage: false,
                cost_usd_observed: true,
            },
            &enforce
        )
        .is_none());
    }

    #[async_trait]
    impl AgentBackend for LeaseLossAfterUsageAgent {
        fn name(&self) -> &str {
            "lease-loss-after-usage-agent"
        }

        fn agent_contract_capabilities(&self) -> AgentContractCapabilities {
            AgentContractCapabilities {
                prompt_only_launch: true,
                pinned_output_schema: true,
                attempt_observation_stream: true,
            }
        }

        fn reports_usage_cost(&self) -> bool {
            true
        }

        async fn execute_stream(
            &self,
            _request: AgentRequest,
            tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> harness_core::error::Result<()> {
            self.lease_lost.send_replace(true);
            for cost_usd in [0.25, f64::NAN] {
                tx.send(AgentEvent::TokenUsage {
                    usage: harness_core::types::TokenUsage {
                        input_tokens: 23,
                        output_tokens: 5,
                        total_tokens: 28,
                        cost_usd,
                    },
                    cost_usd_observed: true,
                })
                .await
                .map_err(|error| {
                    harness_core::error::HarnessError::AgentExecution(error.to_string())
                })?;
            }
            std::future::pending().await
        }
    }

    fn pinned_contract() -> super::super::agent_contract_enforcement::PinnedJobAgentContract {
        let contract: WorkflowAgentContract = serde_json::from_value(json!({
            "input_schema": "harness.semantic_activity_input.v1",
            "output_schema": "harness.semantic_verdict.v1",
            "allowed_outcomes": ["small", "large"],
            "tools": "none",
            "mutation": "forbidden",
            "workspace": "ephemeral_empty",
            "fresh_context": true,
            "max_primary_attempts": 1,
            "max_corrections": 0
        }))
        .expect("test contract is valid");
        let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(
            &serde_json::to_value(&contract).expect("serialize test contract"),
        );
        super::super::agent_contract_enforcement::PinnedJobAgentContract {
            contract,
            prompt: "Classify only the supplied facts.".to_string(),
            input: json!({
                "schema": "harness.semantic_activity_input.v1",
                "subject": {"kind": "test", "identity": "accounting"},
                "facts": {"scope": "small"},
                "provenance": {"/scope": "server"},
                "contract_hash": contract_hash
            }),
            definition_hash: "sha256:accounting-test".to_string(),
        }
    }

    async fn usage_fixture(
        budget_policy: RuntimeBudgetPolicy,
    ) -> anyhow::Result<(tempfile::TempDir, RuntimeUsageContext, TurnId)> {
        let dir = tempfile::tempdir()?;
        let store_path =
            harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime");
        let database_url = crate::test_helpers::test_database_url()?;
        let store = Arc::new(
            WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url))
                .await?
                .with_budget_policy(budget_policy.clone()),
        );
        let suffix = uuid::Uuid::new_v4();
        let context = RuntimeUsageContext {
            store,
            runtime_job_id: format!("agent-contract-accounting-job-{suffix}"),
            command_id: format!("agent-contract-accounting-command-{suffix}"),
            workflow_id: format!("agent-contract-accounting-workflow-{suffix}"),
            agent_run_id: None,
            runtime_kind: RuntimeKind::CodexExec,
            runtime_profile: "codex-accounting".to_string(),
            agent: "accounting-agent".to_string(),
            model: "gpt-test".to_string(),
            project: "/project/accounting".to_string(),
            task_id: Some("task-accounting".to_string()),
            candidate_group_id: None,
            candidate_id: None,
            candidate_index: None,
            candidate_count: None,
            budget_policy,
        };
        let turn_id = TurnId::from_str(&format!("agent-contract-accounting-turn-{suffix}"));
        Ok((dir, context, turn_id))
    }

    #[tokio::test]
    async fn usage_survives_backend_failure_after_report() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let (_dir, context, turn_id) = usage_fixture(RuntimeBudgetPolicy {
            unlimited: true,
            ..RuntimeBudgetPolicy::default()
        })
        .await?;

        let error = execute_agent_contract_attempt(
            Arc::new(AccountingAgent {
                emit_usage: true,
                fail_after_usage: true,
                cost_usd_observed: true,
            }),
            &pinned_contract(),
            None,
            None,
            30,
            None,
            Some((&context, &turn_id)),
            None,
        )
        .await
        .expect_err("the backend failure must remain visible");

        assert!(error
            .to_string()
            .contains("backend failed after reporting usage"));
        let failure = error
            .downcast::<ContractAttemptFailure>()
            .expect("stream failures must retain their partial attempt");
        assert_eq!(failure.attempt.observations.approval_requests, 1);
        assert_eq!(
            failure.attempt.observations.reported_models[0].0,
            "gpt-test"
        );
        assert!(failure
            .attempt
            .observations
            .started_item_kinds
            .iter()
            .any(|kind| kind == "tool_call:forbidden-tool"));
        let usage = context
            .store
            .runtime_usage_for_workflow(&context.workflow_id)
            .await?
            .expect("reported usage must persist before the backend error returns");
        assert_eq!(usage.metrics.reported_total_tokens, Some(18));
        assert_eq!(usage.cost_usd_micros, 125_000);
        Ok(())
    }

    #[tokio::test]
    async fn lease_loss_drains_queued_usage_before_returning() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let (_dir, context, turn_id) = usage_fixture(RuntimeBudgetPolicy {
            unlimited: true,
            ..RuntimeBudgetPolicy::default()
        })
        .await?;
        let (lease_lost, receiver) = tokio::sync::watch::channel(false);

        let error = execute_agent_contract_attempt(
            Arc::new(LeaseLossAfterUsageAgent { lease_lost }),
            &pinned_contract(),
            None,
            None,
            30,
            None,
            Some((&context, &turn_id)),
            Some(receiver),
        )
        .await
        .expect_err("lease loss must cancel the attempt");

        assert!(error.to_string().contains("lease loss"), "{error}");
        let failure = error
            .downcast::<ContractAttemptFailure>()
            .expect("lease loss must retain its partial attempt");
        assert_eq!(
            failure
                .attempt
                .observations
                .token_usage
                .as_ref()
                .expect("queued usage must be observed")
                .total_tokens,
            28
        );
        let usage = context
            .store
            .runtime_usage_for_workflow(&context.workflow_id)
            .await?
            .expect("queued usage must persist during lease-loss cleanup");
        assert_eq!(usage.metrics.reported_total_tokens, Some(28));
        assert_eq!(usage.cost_usd_micros, 250_000);
        Ok(())
    }

    #[tokio::test]
    async fn lease_loss_preserves_budget_stop_before_later_usage_error() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let (_dir, context, turn_id) = usage_fixture(RuntimeBudgetPolicy {
            default_workflow_budget_usd: 0.10,
            enforcement: RuntimeBudgetEnforcement::Enforce,
            ..RuntimeBudgetPolicy::default()
        })
        .await?;
        let (lease_lost, receiver) = tokio::sync::watch::channel(false);

        let error = execute_agent_contract_attempt(
            Arc::new(LeaseLossAfterUsageAgent { lease_lost }),
            &pinned_contract(),
            None,
            None,
            30,
            None,
            Some((&context, &turn_id)),
            Some(receiver),
        )
        .await
        .expect_err("lease-loss cleanup must retain a queued budget stop");
        let failure = error
            .downcast::<ContractAttemptFailure>()
            .expect("lease loss must retain its structured failure");
        assert!(failure.budget_stop.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn enforced_budget_requires_an_observed_usage_event() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let (_dir, context, turn_id) = usage_fixture(RuntimeBudgetPolicy {
            default_workflow_budget_usd: 1.0,
            enforcement: RuntimeBudgetEnforcement::Enforce,
            ..RuntimeBudgetPolicy::default()
        })
        .await?;

        let error = execute_agent_contract_attempt(
            Arc::new(AccountingAgent {
                emit_usage: true,
                fail_after_usage: false,
                cost_usd_observed: false,
            }),
            &pinned_contract(),
            None,
            None,
            30,
            None,
            Some((&context, &turn_id)),
            None,
        )
        .await
        .expect_err("an enforced budget requires observable per-attempt usage");
        assert!(error.to_string().contains("usage"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn enforced_budget_rejects_terminal_verdict_after_usage_crosses_ceiling(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        let (_dir, context, turn_id) = usage_fixture(RuntimeBudgetPolicy {
            default_workflow_budget_usd: 0.10,
            enforcement: RuntimeBudgetEnforcement::Enforce,
            ..RuntimeBudgetPolicy::default()
        })
        .await?;

        let error = execute_agent_contract_attempt(
            Arc::new(AccountingAgent {
                emit_usage: true,
                fail_after_usage: false,
                cost_usd_observed: true,
            }),
            &pinned_contract(),
            None,
            None,
            30,
            None,
            Some((&context, &turn_id)),
            None,
        )
        .await
        .expect_err("a terminal verdict cannot bypass the enforced ceiling");

        assert!(error.to_string().contains("budget"), "{error}");
        let failure = error
            .downcast::<ContractAttemptFailure>()
            .expect("budget exhaustion must retain its structured stop");
        assert_eq!(
            failure
                .budget_stop
                .as_ref()
                .expect("mid-turn ceiling must be classified as a budget stop")
                .budget_usd,
            0.10
        );
        let usage = context
            .store
            .runtime_usage_for_workflow(&context.workflow_id)
            .await?
            .expect("ceiling-crossing usage must persist");
        assert_eq!(usage.cost_usd_micros, 125_000);
        Ok(())
    }
}
