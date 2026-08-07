//! Hard workflow budget ceiling at activity completion (GH-1770 spec §4.4).
//!
//! The pre-dispatch gate in `runtime/dispatcher.rs` defers commands once a
//! workflow has reached its USD budget, but deferral alone leaves the instance
//! looping on backoff with nobody told. This module closes that loop: when the
//! activity completion that crossed the ceiling commits its decision, the
//! workflow stops in `blocked` and requests operator attention instead of
//! scheduling more work.
//!
//! Enforcement follows the same shadow/enforce split as the dispatch gate:
//! `shadow` records a `BudgetShadowDecision` runtime event and keeps the
//! reducer's decision, `enforce` replaces it.

use super::runtime_usage::{
    cost_usd_from_micros, cost_usd_to_micros, runtime_usage_cost_for_workflow_tx,
};
use super::{insert_event_tx, RuntimeBudgetEnforcement, RuntimeBudgetPolicy};
use crate::runtime::model::{ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowInstance};
use crate::runtime::reducer::budget_exhausted_blocked_decision;
use crate::runtime::terminal_state::workflow_terminal_state_for_version;
use serde_json::json;

/// Returns the replacement decision when the completed activity pushed the
/// workflow past its budget ceiling and enforcement is active; `None` when the
/// reducer's own decision stands.
pub(super) async fn budget_ceiling_blocked_decision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    budget_policy: &RuntimeBudgetPolicy,
    instance: &WorkflowInstance,
    source: &str,
    event: &WorkflowEvent,
    decision: &WorkflowDecision,
) -> anyhow::Result<Option<WorkflowDecision>> {
    if budget_policy.unlimited || !decision_schedules_more_work(instance, decision) {
        return Ok(None);
    }
    // Compare in integer micro-dollars, matching the dispatch gate: spend is
    // stored in micros and a float comparison could flip at the boundary.
    let budget_usd = budget_policy.default_workflow_budget_usd;
    let budget_usd_micros = cost_usd_to_micros(budget_usd)?;
    let spent_usd_micros = runtime_usage_cost_for_workflow_tx(tx, &instance.id).await?;
    if spent_usd_micros < budget_usd_micros {
        return Ok(None);
    }

    let spent_usd = cost_usd_from_micros(spent_usd_micros);
    let reason = format!(
        "workflow {} spent {spent_usd:.2} USD, reaching its {budget_usd:.2} USD budget; \
         raise the budget and unblock the workflow to continue",
        instance.id
    );
    let evidence = json!({
        "spent_usd": spent_usd,
        "budget_usd": budget_usd,
        "enforcement": budget_policy.enforcement.as_str(),
    });

    match budget_policy.enforcement {
        RuntimeBudgetEnforcement::Shadow => {
            let mut shadow_evidence = evidence;
            if let Some(shadow_evidence) = shadow_evidence.as_object_mut() {
                shadow_evidence.insert("decision".to_string(), json!("would_block"));
                shadow_evidence.insert("event_id".to_string(), json!(event.id));
                shadow_evidence.insert("reducer_decision".to_string(), json!(decision.decision));
                shadow_evidence
                    .insert("reducer_next_state".to_string(), json!(decision.next_state));
            }
            insert_event_tx(
                tx,
                &instance.id,
                "BudgetShadowDecision",
                source,
                shadow_evidence,
            )
            .await?;
            Ok(None)
        }
        RuntimeBudgetEnforcement::Enforce => {
            let result: ActivityResult =
                serde_json::from_value(event.event.get("activity_result").cloned().ok_or_else(
                    || anyhow::anyhow!("RuntimeJobCompleted event missing activity_result"),
                )?)?;
            Ok(Some(budget_exhausted_blocked_decision(
                instance, event, &result, &reason, evidence,
            )))
        }
    }
}

/// The ceiling only preempts decisions that would keep the workflow running.
/// A decision that already lands in a terminal state or in `blocked` needs no
/// budget intervention — the workflow is stopping either way.
fn decision_schedules_more_work(instance: &WorkflowInstance, decision: &WorkflowDecision) -> bool {
    if decision.next_state == "blocked" {
        return false;
    }
    workflow_terminal_state_for_version(
        &instance.definition_id,
        instance.definition_version,
        &decision.next_state,
    )
    .is_none()
}
