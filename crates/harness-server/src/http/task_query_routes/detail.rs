use super::super::rest_contract::{ContractJson, LegacyJson as Json};
use super::*;
use crate::runtime_projection::{runtime_string_field, RuntimeWorkflowProjection};
use crate::workflow_runtime_submission::{
    runtime_models::{TaskPhase, TaskTerminalClassification},
    runtime_state::{SchedulerOwnerKind, TaskSchedulerState},
};
use harness_protocol::rest::{
    RuntimeTaskDetailErrorResponse, RuntimeTaskDetailResponse, RuntimeTaskSchedulerOwnerResponse,
    RuntimeTaskSchedulerResponse, RuntimeTaskTerminalResponse, RuntimeTaskWorkflowResponse,
};

enum RuntimeProofLookup {
    Missing,
    InFlight(String),
    Terminal(ProofOfWork),
}

pub(crate) async fn get_runtime_submission(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = state.workflow_runtime_store() {
        return error.into_response();
    }
    let task_id = harness_core::types::TaskId(id);
    match runtime_task_response_by_handle(&state, &task_id).await {
        Ok(Some(runtime_task)) => ContractJson(runtime_task).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            ContractJson(RuntimeTaskDetailErrorResponse {
                error: "runtime submission not found".to_string(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!("get_runtime_submission: runtime workflow lookup failed: {error}");
            runtime_detail_internal_server_error()
        }
    }
}

async fn runtime_task_response_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<RuntimeTaskDetailResponse>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
    else {
        return Ok(None);
    };
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let is_runtime_submission = matches!(
        workflow.definition_id.as_str(),
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID
            | harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID
    ) || workflow
        .data
        .get("definition_hash")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|hash| !hash.trim().is_empty());
    if !is_runtime_submission {
        return Ok(None);
    }

    let task_kind = runtime_submission_task_kind(&workflow)?;
    let RuntimeWorkflowProjection {
        task_status,
        failure_kind,
        phase,
        scheduler,
        project_id,
        submission_handle,
        ..
    } = RuntimeWorkflowProjection::from_workflow_with_registry(
        store.definition_registry(),
        &workflow,
    );
    let external_id = runtime_external_id(task_kind, &workflow.data, issue);
    let submission_id = submission_handle
        .map(|handle| handle.0)
        .unwrap_or_else(|| task_id.0.clone());
    let error = runtime_string_field(&workflow.data, "failure_reason");
    let terminal = TaskTerminalInfo::from_status_error(&task_status, error.as_deref());
    let runtime_usage = store.runtime_usage_for_workflow(&workflow.id).await?;
    let cost_usd_observed = runtime_usage.as_ref().map(|usage| usage.cost_usd_observed);
    let token_usage = runtime_usage.map(|usage| harness_core::types::TokenUsage {
        input_tokens: usage.metrics.input_tokens,
        output_tokens: usage.metrics.output_tokens,
        total_tokens: usage.metrics.total_tokens(),
        cost_usd: harness_workflow::runtime::cost_usd_from_micros(usage.cost_usd_micros),
    });
    let pending_approvals = state
        .core
        .server
        .thread_manager
        .pending_approval_items_for_runtime_handle(&submission_id);
    let description = Some(super::runtime_submissions::runtime_submission_description(
        &workflow, task_kind, issue,
    ));
    Ok(Some(RuntimeTaskDetailResponse {
        id: submission_id.clone(),
        task_id: submission_id.clone(),
        submission_id,
        task_kind: task_kind.as_ref().to_string(),
        status: task_status.as_str().to_string(),
        workflow_state: workflow.state.clone(),
        failure_kind: failure_kind.map(|kind| kind.as_ref().to_string()),
        phase: task_phase_name(phase).to_string(),
        scheduler: runtime_task_scheduler_response(scheduler),
        turn: 0,
        pr_url: runtime_string_field(&workflow.data, "pr_url"),
        description,
        created_at: workflow.created_at.to_rfc3339(),
        updated_at: workflow.updated_at.to_rfc3339(),
        execution_path: "workflow_runtime",
        workflow_id: workflow.id.clone(),
        source: runtime_string_field(&workflow.data, "source"),
        external_id,
        tracker_source: runtime_string_field(&workflow.data, "tracker_source"),
        tracker_external_id: runtime_string_field(&workflow.data, "tracker_external_id"),
        repo: runtime_string_field(&workflow.data, "repo"),
        project: project_id,
        issue,
        error,
        token_usage,
        cost_usd_observed,
        pending_approvals,
        terminal: terminal.map(runtime_task_terminal_response),
        depends_on: runtime_task_id_array(&workflow.data, "depends_on"),
        subtask_ids: Vec::new(),
        workflow: Some(runtime_task_workflow_response(
            TaskWorkflowSummary::from_runtime(&workflow),
        )),
    }))
}

fn task_phase_name(phase: TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Triage => "triage",
        TaskPhase::Plan => "plan",
        TaskPhase::Implement => "implement",
        TaskPhase::Review => "review",
        TaskPhase::Terminal => "terminal",
    }
}

fn runtime_task_scheduler_response(scheduler: TaskSchedulerState) -> RuntimeTaskSchedulerResponse {
    RuntimeTaskSchedulerResponse {
        authority_state: scheduler.authority_state.as_str().to_string(),
        owner: scheduler
            .owner
            .map(|owner| RuntimeTaskSchedulerOwnerResponse {
                kind: match owner.kind {
                    SchedulerOwnerKind::Scheduler => "scheduler",
                    SchedulerOwnerKind::RuntimeHost => "runtime_host",
                }
                .to_string(),
                id: owner.id,
            }),
        run_generation: scheduler.run_generation,
        recovery_generation: scheduler.recovery_generation,
        lease_expires_at: scheduler.lease_expires_at,
    }
}

fn runtime_task_terminal_response(terminal: TaskTerminalInfo) -> RuntimeTaskTerminalResponse {
    RuntimeTaskTerminalResponse {
        status: terminal.status.as_str().to_string(),
        classification: match terminal.classification {
            TaskTerminalClassification::Done => "done",
            TaskTerminalClassification::Failed => "failed",
            TaskTerminalClassification::Stalled => "stalled",
            TaskTerminalClassification::Cancelled => "cancelled",
        }
        .to_string(),
        reason: terminal.reason,
        rounds_used: terminal.rounds_used,
        last_status: terminal
            .last_status
            .map(|status| status.as_str().to_string()),
        waiting_on: terminal.waiting_on,
    }
}

fn runtime_task_workflow_response(workflow: TaskWorkflowSummary) -> RuntimeTaskWorkflowResponse {
    RuntimeTaskWorkflowResponse {
        id: workflow.id,
        definition_id: workflow.definition_id,
        state: workflow.state,
        project_id: workflow.project_id,
        issue_number: workflow.issue_number,
        pr_number: workflow.pr_number,
        force_execute: workflow.force_execute,
        plan_concern: workflow.plan_concern,
    }
}

pub(crate) fn proof_from_runtime_workflow(
    registry: &harness_workflow::runtime::WorkflowDefinitionRegistry,
    task_id: &harness_core::types::TaskId,
    workflow: &harness_workflow::runtime::WorkflowInstance,
    events: &[harness_workflow::runtime::WorkflowEvent],
    decisions: &[harness_workflow::runtime::WorkflowDecisionRecord],
) -> ProofOfWork {
    let projection = RuntimeWorkflowProjection::from_workflow_with_registry(registry, workflow);
    let status = projection.task_status;
    let pr_url = runtime_string_field(&workflow.data, "pr_url")
        .or_else(|| runtime_string_field(&workflow.data, "last_pr_url"));
    let accepted_decisions = decisions
        .iter()
        .filter(|record| record.accepted)
        .collect::<Vec<_>>();
    let approved = events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "PrReadyToMerge" | "MergeApproved" | "PrMerged"
        )
    }) || accepted_decisions.iter().any(|record| {
        matches!(
            record.decision.decision.as_str(),
            "mark_ready_to_merge"
                | "quality_gate_passed"
                | "approve_merge"
                | "record_pr_merged"
                | "quality_passed"
        )
    }) || workflow.state == "passed";
    let changes_requested = events
        .iter()
        .any(|event| event.event_type == "FeedbackFound")
        || accepted_decisions.iter().any(|record| {
            matches!(
                record.decision.decision.as_str(),
                "address_pr_feedback" | "await_feedback_after_rework"
            )
        });
    let review_outcome = if approved {
        ReviewOutcome::Approved
    } else if changes_requested {
        ReviewOutcome::ChangesRequested
    } else {
        ReviewOutcome::Skipped
    };
    let ci_status = if status == TaskStatus::Failed {
        CiStatus::Failed
    } else if status == TaskStatus::Done && review_outcome == ReviewOutcome::Approved {
        CiStatus::Passed
    } else {
        CiStatus::Unknown
    };
    let review_event_count = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "FeedbackFound" | "NoFeedbackFound" | "PrReadyToMerge"
            )
        })
        .count();
    let review_decision_count = accepted_decisions
        .iter()
        .filter(|record| {
            matches!(
                record.decision.decision.as_str(),
                "address_pr_feedback"
                    | "wait_for_pr_feedback"
                    | "mark_ready_to_merge"
                    | "start_quality_gate"
                    | "quality_gate_passed"
                    | "quality_passed"
            )
        })
        .count();
    let mut quality_signals = vec![
        QualitySignal {
            name: "workflow_id".to_string(),
            value: workflow.id.clone(),
        },
        QualitySignal {
            name: "workflow_state".to_string(),
            value: workflow.state.clone(),
        },
    ];
    if let Some(error) = runtime_string_field(&workflow.data, "failure_reason") {
        quality_signals.push(QualitySignal {
            name: "error".to_string(),
            value: error,
        });
    }
    ProofOfWork {
        task_id: task_id.as_str().to_string(),
        status: status.as_ref().to_string(),
        pr_url,
        ci_status,
        review_outcome,
        review_rounds: review_event_count.max(review_decision_count) as u32,
        quality_signals,
    }
}

async fn runtime_proof_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<RuntimeProofLookup> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let Some(workflow) = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
    else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let status = RuntimeWorkflowProjection::from_workflow_with_registry(
        store.definition_registry(),
        &workflow,
    )
    .task_status;
    if !status.is_terminal() {
        return Ok(RuntimeProofLookup::InFlight(status.as_ref().to_string()));
    }
    let events = store.events_for(&workflow.id).await?;
    let decisions = store.decisions_for(&workflow.id).await?;
    let proof_task_id = crate::workflow_runtime_submission::runtime_issue_task_handle(&workflow)
        .unwrap_or_else(|| task_id.clone());
    Ok(RuntimeProofLookup::Terminal(proof_from_runtime_workflow(
        store.definition_registry(),
        &proof_task_id,
        &workflow,
        &events,
        &decisions,
    )))
}

pub(crate) async fn get_runtime_submission_proof(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = state.workflow_runtime_store() {
        return error.into_response();
    }
    let task_id = harness_core::types::TaskId(id);
    match runtime_proof_by_handle(&state, &task_id).await {
        Ok(RuntimeProofLookup::Terminal(proof)) => Json(proof).into_response(),
        Ok(RuntimeProofLookup::InFlight(status)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "runtime submission is not in a terminal state",
                "status": status,
            })),
        )
            .into_response(),
        Ok(RuntimeProofLookup::Missing) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime submission not found"})),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(
                "get_runtime_submission_proof: runtime workflow lookup failed: {error}"
            );
            runtime_detail_internal_server_error()
        }
    }
}

fn runtime_detail_internal_server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal server error"})),
    )
        .into_response()
}
