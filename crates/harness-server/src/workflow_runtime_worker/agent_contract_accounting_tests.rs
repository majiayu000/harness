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
    fail_after_usage: bool,
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

    async fn execute_stream(
        &self,
        _request: AgentRequest,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        tx.send(AgentEvent::TokenUsage {
            usage: harness_core::types::TokenUsage {
                input_tokens: 11,
                output_tokens: 7,
                total_tokens: 18,
                cost_usd: 0.125,
            },
        })
        .await
        .map_err(|error| harness_core::error::HarnessError::AgentExecution(error.to_string()))?;
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
        .map_err(|error| harness_core::error::HarnessError::AgentExecution(error.to_string()))?;
        Ok(())
    }
}

struct LeaseLossAfterUsageAgent {
    lease_lost: tokio::sync::watch::Sender<bool>,
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

    async fn execute_stream(
        &self,
        _request: AgentRequest,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        tx.send(AgentEvent::TokenUsage {
            usage: harness_core::types::TokenUsage {
                input_tokens: 23,
                output_tokens: 5,
                total_tokens: 28,
                cost_usd: 0.25,
            },
        })
        .await
        .map_err(|error| harness_core::error::HarnessError::AgentExecution(error.to_string()))?;
        self.lease_lost.send_replace(true);
        std::future::pending().await
    }
}

fn pinned_contract(
) -> crate::workflow_runtime_worker::agent_contract_enforcement::PinnedJobAgentContract {
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
    crate::workflow_runtime_worker::agent_contract_enforcement::PinnedJobAgentContract {
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
    let store_path = harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime");
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
            fail_after_usage: true,
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
        .downcast::<super::super::agent_contract_stream::ContractAttemptFailure>()
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
        .downcast::<super::super::agent_contract_stream::ContractAttemptFailure>()
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
async fn enforced_budget_rejects_terminal_verdict_after_usage_crosses_ceiling() -> anyhow::Result<()>
{
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
            fail_after_usage: false,
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
    let usage = context
        .store
        .runtime_usage_for_workflow(&context.workflow_id)
        .await?
        .expect("ceiling-crossing usage must persist");
    assert_eq!(usage.cost_usd_micros, 125_000);
    Ok(())
}
