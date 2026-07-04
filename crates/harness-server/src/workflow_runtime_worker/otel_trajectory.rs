use crate::http::AppState;
use harness_core::types::{ThreadId, TurnId};
use harness_observe::otel_trajectory::{
    ActivitySpan, AgentTurnSpan, TrajectoryTraceContext, WorkflowRootSpan,
};
use harness_workflow::runtime::{
    ActivityResult, ActivityStatus, RuntimeJob, RuntimeProfile, WorkflowInstance,
    WorkflowRuntimeStore,
};
use serde_json::Value;
use std::sync::Arc;

pub(super) async fn emit_runtime_job_trajectory_completion(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
) -> anyhow::Result<()> {
    if !state.core.server.config.otel.trajectory {
        return Ok(());
    }
    let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(workflow) = store.get_instance(workflow_id).await? else {
        return Ok(());
    };
    let Some(trace_context) = store.ensure_otel_trace_context(workflow_id).await? else {
        return Ok(());
    };
    let Some(trace_context) =
        TrajectoryTraceContext::new(trace_context.trace_id, trace_context.root_span_id)
    else {
        anyhow::bail!("workflow {workflow_id} has invalid OTel trace context");
    };
    let Some(result) = job
        .output
        .as_ref()
        .and_then(|output| serde_json::from_value::<ActivityResult>(output.clone()).ok())
    else {
        return Ok(());
    };

    let outcome = activity_status_label(result.status).to_string();
    let retry_attempt = retry_attempt(job);
    state
        .observability
        .events
        .record_trajectory_activity(ActivitySpan {
            trace_context: trace_context.clone(),
            workflow_id: workflow_id.to_string(),
            runtime_job_id: job.id.clone(),
            activity_kind: result.activity.clone(),
            outcome: outcome.clone(),
            retry_attempt,
            started_at: Some(job.created_at),
            ended_at: Some(job.updated_at),
        });

    if let Some(turn_ref) = runtime_turn_artifact(&result) {
        if let Some(turn) = state.core.server.thread_manager.get_turn(
            &ThreadId::from_str(&turn_ref.thread_id),
            &TurnId::from_str(&turn_ref.turn_id),
        ) {
            let runtime_profile = otel_runtime_profile_from_job(job);
            state
                .observability
                .events
                .record_trajectory_agent_turn(AgentTurnSpan {
                    trace_context: trace_context.clone(),
                    workflow_id: workflow_id.to_string(),
                    runtime_job_id: job.id.clone(),
                    activity_kind: result.activity.clone(),
                    outcome,
                    system: turn_ref.agent,
                    model: runtime_profile.and_then(|profile| profile.model),
                    input_tokens: Some(turn.token_usage.input_tokens),
                    output_tokens: Some(turn.token_usage.output_tokens),
                    cost_usd: Some(turn.token_usage.cost_usd),
                    thread_id: turn_ref.thread_id,
                    turn_id: turn_ref.turn_id,
                    retry_attempt,
                    started_at: Some(turn.started_at),
                    ended_at: turn.completed_at,
                });
        }
    }

    if workflow.is_terminal() {
        state
            .observability
            .events
            .record_trajectory_workflow_root(workflow_root_span(trace_context, &workflow));
    }
    Ok(())
}

fn workflow_root_span(
    trace_context: TrajectoryTraceContext,
    workflow: &WorkflowInstance,
) -> WorkflowRootSpan {
    WorkflowRootSpan {
        trace_context,
        workflow_id: workflow.id.clone(),
        definition_id: workflow.definition_id.clone(),
        subject_type: workflow.subject.subject_type.clone(),
        subject_key: workflow.subject.subject_key.clone(),
        outcome: workflow.state.clone(),
        started_at: Some(workflow.created_at),
        ended_at: Some(workflow.updated_at),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeTurnRef {
    thread_id: String,
    turn_id: String,
    agent: String,
}

fn runtime_turn_artifact(result: &ActivityResult) -> Option<RuntimeTurnRef> {
    let artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_turn")?;
    let thread_id = artifact.artifact.get("thread_id")?.as_str()?.trim();
    let turn_id = artifact.artifact.get("turn_id")?.as_str()?.trim();
    let agent = artifact.artifact.get("agent")?.as_str()?.trim();
    if thread_id.is_empty() || turn_id.is_empty() || agent.is_empty() {
        return None;
    }
    Some(RuntimeTurnRef {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        agent: agent.to_string(),
    })
}

fn otel_runtime_profile_from_job(job: &RuntimeJob) -> Option<RuntimeProfile> {
    serde_json::from_value(job.input.get("runtime_profile")?.clone()).ok()
}

fn retry_attempt(job: &RuntimeJob) -> Option<u64> {
    (job.lease_generation > 1).then_some(job.lease_generation - 1)
}

fn activity_status_label(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Succeeded => "succeeded",
        ActivityStatus::Failed => "failed",
        ActivityStatus::Blocked => "blocked",
        ActivityStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::ActivityArtifact;
    use serde_json::json;

    #[test]
    fn otel_turn_spans_extract_runtime_turn_artifact() {
        let result =
            ActivityResult::succeeded("implement", "done").with_artifact(ActivityArtifact::new(
                "runtime_turn",
                json!({
                    "thread_id": "thread-1",
                    "turn_id": "turn-1",
                    "agent": "codex",
                }),
            ));

        assert_eq!(
            runtime_turn_artifact(&result),
            Some(RuntimeTurnRef {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                agent: "codex".to_string(),
            })
        );
    }

    #[test]
    fn otel_turn_spans_skip_incomplete_runtime_turn_artifact() {
        let result =
            ActivityResult::succeeded("implement", "done").with_artifact(ActivityArtifact::new(
                "runtime_turn",
                json!({
                    "thread_id": "thread-1",
                    "turn_id": "",
                    "agent": "codex",
                }),
            ));

        assert_eq!(runtime_turn_artifact(&result), None);
    }
}
