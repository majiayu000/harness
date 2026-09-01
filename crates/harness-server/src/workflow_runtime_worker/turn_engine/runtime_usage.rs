use harness_core::agent::AgentBackend;
use harness_core::config::workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy};
use harness_core::run_id::RunId;
use harness_core::types::{TokenUsage, TurnId};
use harness_workflow::runtime::{
    cost_usd_from_micros, cost_usd_to_micros, RuntimeKind, RuntimeUsageMetrics, RuntimeUsageUpsert,
    RuntimeUsageUpsertOutcome, WorkflowRuntimeStore,
};
use serde_json::json;
use std::sync::Arc;

/// Mid-turn budget stop (GH-1770 spec §4.3): the streamed usage that was just
/// persisted put the workflow at or over its USD ceiling, so the in-flight
/// turn must be interrupted rather than allowed to keep spending.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnBudgetStop {
    pub(crate) workflow_id: String,
    pub(crate) spent_usd: f64,
    pub(crate) budget_usd: f64,
}

pub(crate) fn budget_stop_artifact(
    stop: &TurnBudgetStop,
) -> harness_workflow::runtime::ActivityArtifact {
    harness_workflow::runtime::ActivityArtifact::new(
        harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_BUDGET_STOP,
        serde_json::json!({
            "workflow_id": stop.workflow_id,
            "spent_usd": stop.spent_usd,
            "budget_usd": stop.budget_usd,
            "enforcement": "enforce",
        }),
    )
}

pub(crate) fn enforced_budget_cost_error(
    backend: &dyn AgentBackend,
    policy: &RuntimeBudgetPolicy,
) -> Option<String> {
    (!policy.unlimited
        && policy.enforcement == RuntimeBudgetEnforcement::Enforce
        && !backend.reports_usage_cost())
    .then(|| {
        format!(
            "agent backend `{}` does not report USD cost; refusing to launch an agent contract under enforced USD budget policy",
            backend.name()
        )
    })
}

#[derive(Clone)]
pub(crate) struct RuntimeUsageContext {
    pub(crate) store: Arc<WorkflowRuntimeStore>,
    pub(crate) runtime_job_id: String,
    pub(crate) command_id: String,
    pub(crate) workflow_id: String,
    pub(crate) agent_run_id: Option<RunId>,
    pub(crate) runtime_kind: RuntimeKind,
    pub(crate) runtime_profile: String,
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) project: String,
    pub(crate) task_id: Option<String>,
    pub(crate) candidate_group_id: Option<String>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) candidate_index: Option<u32>,
    pub(crate) candidate_count: Option<u32>,
    pub(crate) budget_policy: RuntimeBudgetPolicy,
}

impl std::fmt::Debug for RuntimeUsageContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeUsageContext")
            .field("runtime_job_id", &self.runtime_job_id)
            .field("command_id", &self.command_id)
            .field("workflow_id", &self.workflow_id)
            .field("runtime_kind", &self.runtime_kind)
            .field("runtime_profile", &self.runtime_profile)
            .field("agent", &self.agent)
            .field("model", &self.model)
            .field("project", &self.project)
            .field("task_id", &self.task_id)
            .field("candidate_group_id", &self.candidate_group_id)
            .field("candidate_id", &self.candidate_id)
            .field("candidate_index", &self.candidate_index)
            .field("candidate_count", &self.candidate_count)
            .finish_non_exhaustive()
    }
}

impl RuntimeUsageContext {
    pub(crate) async fn persist_agent_run_start(&self, turn_id: &TurnId) -> anyhow::Result<()> {
        self.store
            .upsert_runtime_agent_run(&RuntimeUsageUpsert {
                runtime_job_id: self.runtime_job_id.clone(),
                command_id: self.command_id.clone(),
                workflow_id: self.workflow_id.clone(),
                turn_id: Some(turn_id.as_str().to_string()),
                agent_run_id: self.agent_run_id.clone(),
                runtime_kind: self.runtime_kind,
                runtime_profile: self.runtime_profile.clone(),
                agent: self.agent.clone(),
                model: self.model.clone(),
                project: self.project.clone(),
                task_id: self.task_id.clone(),
                candidate_group_id: self.candidate_group_id.clone(),
                candidate_id: self.candidate_id.clone(),
                candidate_index: self.candidate_index,
                candidate_count: self.candidate_count,
                metrics: RuntimeUsageMetrics::default(),
                cost_usd_micros: 0,
                cost_usd_observed: false,
                reported_at: chrono::Utc::now(),
            })
            .await
    }

    pub(crate) async fn persist_token_usage(
        &self,
        turn_id: &TurnId,
        usage: &TokenUsage,
        cost_usd_observed: bool,
    ) -> anyhow::Result<()> {
        match self
            .store
            .upsert_runtime_usage(&RuntimeUsageUpsert {
                runtime_job_id: self.runtime_job_id.clone(),
                command_id: self.command_id.clone(),
                workflow_id: self.workflow_id.clone(),
                turn_id: Some(turn_id.as_str().to_string()),
                agent_run_id: self.agent_run_id.clone(),
                runtime_kind: self.runtime_kind,
                runtime_profile: self.runtime_profile.clone(),
                agent: self.agent.clone(),
                model: self.model.clone(),
                project: self.project.clone(),
                task_id: self.task_id.clone(),
                candidate_group_id: self.candidate_group_id.clone(),
                candidate_id: self.candidate_id.clone(),
                candidate_index: self.candidate_index,
                candidate_count: self.candidate_count,
                metrics: RuntimeUsageMetrics::from_token_usage(usage),
                cost_usd_micros: cost_usd_to_micros(usage.cost_usd)?,
                cost_usd_observed,
                reported_at: chrono::Utc::now(),
            })
            .await?
        {
            RuntimeUsageUpsertOutcome::SkippedZeroUsage => {}
            RuntimeUsageUpsertOutcome::Persisted => {}
        }
        Ok(())
    }

    pub(crate) async fn budget_stop(&self) -> anyhow::Result<Option<TurnBudgetStop>> {
        if self.budget_policy.unlimited {
            return Ok(None);
        }
        let budget_usd = self.budget_policy.default_workflow_budget_usd;
        let budget_usd_micros = cost_usd_to_micros(budget_usd)?;
        let spent_usd_micros = self
            .store
            .runtime_usage_for_workflow(&self.workflow_id)
            .await?
            .map(|usage| usage.cost_usd_micros)
            .unwrap_or(0);
        if spent_usd_micros < budget_usd_micros {
            return Ok(None);
        }
        let spent_usd = cost_usd_from_micros(spent_usd_micros);
        match self.budget_policy.enforcement {
            RuntimeBudgetEnforcement::Shadow => {
                self.store
                    .append_event(
                        &self.workflow_id,
                        "BudgetShadowDecision",
                        "workflow_runtime_turn_watchdog",
                        json!({
                            "decision": "would_interrupt",
                            "spent_usd": spent_usd,
                            "budget_usd": budget_usd,
                            "runtime_job_id": self.runtime_job_id,
                            "command_id": self.command_id,
                        }),
                    )
                    .await?;
                Ok(None)
            }
            RuntimeBudgetEnforcement::Enforce => Ok(Some(TurnBudgetStop {
                workflow_id: self.workflow_id.clone(),
                spent_usd,
                budget_usd,
            })),
        }
    }
}
