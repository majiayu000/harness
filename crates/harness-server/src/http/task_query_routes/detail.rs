use super::*;

/// Response type for `GET /tasks/{id}` — `TaskState` fields plus the optional workflow summary
/// that requires a separate workflow-store lookup (not persisted on `TaskState` itself).
#[derive(Serialize)]
struct FullTaskResponse {
    #[serde(flatten)]
    inner: crate::task_runner::TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<TaskWorkflowSummary>,
}

#[derive(Serialize)]
struct RuntimeTaskResponse {
    id: String,
    task_id: String,
    task_kind: TaskKind,
    status: String,
    execution_path: &'static str,
    workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<TaskWorkflowSummary>,
}

enum RuntimeProofLookup {
    Missing,
    InFlight(String),
    Terminal(ProofOfWork),
}

pub(crate) async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(task)) => {
            let workflow = enrich_task_workflow(&state, &task).await;
            Json(FullTaskResponse {
                inner: task,
                workflow,
            })
            .into_response()
        }
        Ok(None) => {
            if workflow_runtime_store_unavailable(&state) {
                return workflow_runtime_store_unavailable_response();
            }
            match runtime_task_response_by_handle(&state, &task_id).await {
                Ok(Some(runtime_task)) => Json(runtime_task).into_response(),
                Ok(None) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "task not found"})),
                )
                    .into_response(),
                Err(e) => {
                    tracing::error!("get_task: runtime workflow lookup failed: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!("get_task: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

async fn runtime_task_response_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<RuntimeTaskResponse>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(None);
    };
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let Some(task_kind) = (match workflow.definition_id.as_str() {
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => Some(TaskKind::Issue),
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID => Some(TaskKind::Prompt),
        _ => None,
    }) else {
        return Ok(None);
    };
    let external_id = runtime_external_id(task_kind, &workflow.data, issue);
    Ok(Some(RuntimeTaskResponse {
        id: task_id.as_str().to_string(),
        task_id: task_id.as_str().to_string(),
        task_kind,
        status: workflow.state.clone(),
        execution_path: "workflow_runtime",
        workflow_id: workflow.id.clone(),
        external_id,
        repo: workflow
            .data
            .get("repo")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        project: workflow
            .data
            .get("project_id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        issue,
        workflow: Some(TaskWorkflowSummary::from_runtime(&workflow)),
    }))
}

fn workflow_runtime_store_unavailable(state: &AppState) -> bool {
    state.core.workflow_runtime_store.is_none() && workflow_runtime_store_required(state)
}

fn workflow_runtime_store_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "workflow runtime store unavailable",
            "message": "workflow runtime store is unavailable",
        })),
    )
        .into_response()
}

/// Look up the issue-workflow instance for a task using targeted store queries.
/// Returns `None` when the workflow store is unavailable or the task has no workflow association.
async fn enrich_task_workflow(
    state: &AppState,
    task: &crate::task_runner::TaskState,
) -> Option<TaskWorkflowSummary> {
    let workflow_store = state.core.issue_workflow_store.as_ref()?;
    let project_id = task.project_root.as_ref()?.to_string_lossy().into_owned();

    let by_issue = task
        .external_id
        .as_deref()
        .and_then(|id| id.strip_prefix("issue:"))
        .and_then(|n| n.parse::<u64>().ok());
    let by_pr = task
        .external_id
        .as_deref()
        .and_then(|id| id.strip_prefix("pr:"))
        .and_then(|n| n.parse::<u64>().ok())
        .or_else(|| {
            task.pr_url
                .as_deref()
                .and_then(super::super::parse_pr_num_from_url)
        });

    match (by_issue, by_pr) {
        (Some(issue), _) => workflow_store
            .get_by_issue(&project_id, task.repo.as_deref(), issue)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("get_task: workflow lookup by issue failed: {e}");
                None
            })
            .map(TaskWorkflowSummary::from),
        (None, Some(pr)) => workflow_store
            .get_by_pr(&project_id, task.repo.as_deref(), pr)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("get_task: workflow lookup by PR failed: {e}");
                None
            })
            .map(TaskWorkflowSummary::from),
        (None, None) => None,
    }
}

/// GET /tasks/{id}/artifacts — all persisted artifacts for a task.
pub(crate) async fn get_task_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_artifacts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.list_artifacts(&task_id).await {
        Ok(artifacts) => Json(artifacts).into_response(),
        Err(e) => {
            tracing::error!("get_task_artifacts: list artifacts error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Derive a [`ProofOfWork`] summary from a [`TaskState`].
///
/// Uses only `TaskState` fields already populated by the review loop, so the
/// derivation is a pure function and stays cheap to call from HTTP handlers.
///
/// CI status policy:
/// - `Passed` requires both terminal `Done` status and an approving review,
///   because the LGTM gate is what runs the project's test command.
/// - `Failed` covers explicit `Failed` status and review rounds that recorded
///   a `quota_exhausted` / `billing_failed` / `upstream_failure` / `timeout`
///   result.
/// - Everything else (cancelled, no review evidence) reports `Unknown`.
pub(crate) fn proof_from_state(task: &TaskState) -> ProofOfWork {
    let mut review_rounds: u32 = 0;
    let mut last_review_result: Option<&str> = None;
    let mut saw_review_failure = false;

    for round in &task.rounds {
        if !matches!(round.action.as_str(), "review" | "agent_review") {
            continue;
        }
        review_rounds = review_rounds.saturating_add(1);
        last_review_result = Some(round.result.as_str());
        if matches!(
            round.result.as_str(),
            "quota_exhausted" | "billing_failed" | "upstream_failure" | "timeout" | "failed"
        ) {
            saw_review_failure = true;
        }
    }

    let review_outcome = match last_review_result {
        Some("lgtm") | Some("approved") => ReviewOutcome::Approved,
        Some("needs_fix") | Some("fixed") => ReviewOutcome::ChangesRequested,
        Some(result) if result.ends_with(" issues") => ReviewOutcome::ChangesRequested,
        Some(_) => ReviewOutcome::Skipped,
        None => ReviewOutcome::Skipped,
    };

    let ci_status =
        if matches!(task.status, TaskStatus::Done) && review_outcome == ReviewOutcome::Approved {
            CiStatus::Passed
        } else if matches!(task.status, TaskStatus::Failed) || saw_review_failure {
            CiStatus::Failed
        } else {
            CiStatus::Unknown
        };

    let mut signals = Vec::new();
    signals.push(QualitySignal {
        name: "turns".to_string(),
        value: task.turn.to_string(),
    });
    if let Some(error) = task.error.as_deref().filter(|s| !s.is_empty()) {
        signals.push(QualitySignal {
            name: "error".to_string(),
            value: error.to_string(),
        });
    }

    ProofOfWork {
        task_id: task.id.0.clone(),
        status: task.status.as_ref().to_string(),
        pr_url: task.pr_url.clone(),
        ci_status,
        review_outcome,
        review_rounds,
        quality_signals: signals,
    }
}

pub(crate) fn proof_from_runtime_workflow(
    task_id: &harness_core::types::TaskId,
    workflow: &harness_workflow::runtime::WorkflowInstance,
    events: &[harness_workflow::runtime::WorkflowEvent],
    decisions: &[harness_workflow::runtime::WorkflowDecisionRecord],
) -> ProofOfWork {
    let status = runtime_workflow_state_to_task_status(&workflow.state);
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
            "mark_ready_to_merge" | "approve_merge" | "record_pr_merged" | "quality_passed"
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
                    | "quality_passed"
            )
        })
        .count();
    let review_rounds = review_event_count.max(review_decision_count) as u32;

    let mut signals = vec![
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
        signals.push(QualitySignal {
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
        review_rounds,
        quality_signals: signals,
    }
}

async fn runtime_proof_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<RuntimeProofLookup> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let Some(workflow) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let status = runtime_workflow_state_to_task_status(&workflow.state);
    if !status.is_terminal() {
        return Ok(RuntimeProofLookup::InFlight(status.as_ref().to_string()));
    }
    let events = store.events_for(&workflow.id).await?;
    let decisions = store.decisions_for(&workflow.id).await?;
    Ok(RuntimeProofLookup::Terminal(proof_from_runtime_workflow(
        task_id, &workflow, &events, &decisions,
    )))
}

/// GET /tasks/{id}/proof — machine-readable proof-of-work summary for a
/// completed task.
///
/// Returns 404 for unknown task IDs, 422 while the task is still in flight,
/// and 200 with a JSON [`ProofOfWork`] for terminal tasks (Done / Failed /
/// Cancelled). See `harness-core::proof_of_work` for the wire format.
pub(crate) async fn get_task_proof(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(task)) => {
            if !task.status.is_terminal() {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "error": "task is not in a terminal state",
                        "status": task.status.as_ref(),
                    })),
                )
                    .into_response();
            }
            Json(proof_from_state(&task)).into_response()
        }
        Ok(None) => {
            if workflow_runtime_store_unavailable(&state) {
                return workflow_runtime_store_unavailable_response();
            }
            match runtime_proof_by_handle(&state, &task_id).await {
                Ok(RuntimeProofLookup::Terminal(proof)) => Json(proof).into_response(),
                Ok(RuntimeProofLookup::InFlight(status)) => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "error": "task is not in a terminal state",
                        "status": status,
                    })),
                )
                    .into_response(),
                Ok(RuntimeProofLookup::Missing) => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "task not found"})),
                )
                    .into_response(),
                Err(e) => {
                    tracing::error!("get_task_proof: runtime workflow lookup failed: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!("get_task_proof: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /tasks/{id}/prompts — all persisted redacted prompts for a task.
pub(crate) async fn get_task_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_prompts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.get_prompts(&task_id).await {
        Ok(prompts) => Json(prompts).into_response(),
        Err(e) => {
            tracing::error!("get_task_prompts: query error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
