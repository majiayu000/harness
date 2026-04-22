use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::proof_of_work::{
    CiStatus, ProofOfWork, QualitySignal, ReviewOutcome, ACTION_AGENT_REVIEW, ACTION_REVIEW,
    RESULT_APPROVED, RESULT_COMPLETED, RESULT_LGTM, RESULT_QUOTA_EXHAUSTED,
};
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{RoundResult, TaskState, TaskStatus};

/// Derive a `ProofOfWork` from a `TaskState`.
///
/// Scans `rounds` in reverse to find the final review detail, counts review
/// rounds, and infers outcomes from known result label constants.
fn proof_from_state(state: &TaskState) -> ProofOfWork {
    // Accept both "review" (external bot) and "agent_review" (agent gate when
    // review_bot_auto_trigger is disabled) so neither path is silently dropped.
    let review_rounds_vec: Vec<&RoundResult> = state
        .rounds
        .iter()
        .filter(|r| r.action == ACTION_REVIEW || r.action == ACTION_AGENT_REVIEW)
        .collect();

    let review_rounds = review_rounds_vec.len() as u32;

    // LGTM: external bot writes "lgtm"; agent reviewer writes "approved".
    let has_lgtm = review_rounds_vec.iter().any(|r| {
        r.result == RESULT_LGTM || (r.action == ACTION_AGENT_REVIEW && r.result == RESULT_APPROVED)
    });
    // Non-PR review tasks (periodic_review, sprint_planner) write result="completed";
    // treat them as not applicable rather than falsely reporting changes_requested.
    let is_review_only = review_rounds > 0
        && review_rounds_vec
            .iter()
            .all(|r| r.result == RESULT_COMPLETED);

    // Quota-heuristic graduation: external reviewer quota was exhausted on every
    // round and the test gate passed as a proxy — task is Done, tests passed.
    let quota_heuristic_done = review_rounds > 0
        && review_rounds_vec
            .iter()
            .all(|r| r.result == RESULT_QUOTA_EXHAUSTED)
        && state.status == TaskStatus::Done;

    let review_outcome = if is_review_only {
        ReviewOutcome::NotApplicable
    } else if quota_heuristic_done || has_lgtm {
        ReviewOutcome::Approved
    } else if review_rounds == 0 {
        ReviewOutcome::Skipped
    } else {
        ReviewOutcome::ChangesRequested
    };

    // CI status: Passed only when approved and Done; Failed when task Failed.
    let ci_status = match &state.status {
        TaskStatus::Done if review_outcome == ReviewOutcome::Approved => CiStatus::Passed,
        TaskStatus::Done => CiStatus::Unknown,
        TaskStatus::Failed => CiStatus::Failed,
        _ => CiStatus::Unknown,
    };

    // Detail from the last review round (either action type) that has a detail field.
    let final_review_detail = state
        .rounds
        .iter()
        .rev()
        .find(|r| {
            (r.action == ACTION_REVIEW || r.action == ACTION_AGENT_REVIEW) && r.detail.is_some()
        })
        .and_then(|r| r.detail.clone());

    // Basic quality signals derived from the task.
    let mut signals: Vec<QualitySignal> = vec![
        QualitySignal {
            label: "review_rounds".to_string(),
            value: review_rounds.to_string(),
        },
        QualitySignal {
            label: "pr_url".to_string(),
            value: if state.pr_url.is_some() {
                "present".to_string()
            } else {
                "absent".to_string()
            },
        },
    ];
    if let Some(note) = &state.error {
        signals.push(QualitySignal {
            label: "completion_note".to_string(),
            value: note.clone(),
        });
    }

    ProofOfWork {
        task_id: state.id.0.clone(),
        pr_url: state.pr_url.clone(),
        repo: state.repo.clone(),
        issue: state.issue,
        ci_status,
        review_outcome,
        review_rounds,
        final_review_detail,
        quality_signals: signals,
    }
}

/// GET /tasks/{id}/proof — machine-readable proof-of-work for a terminal task.
///
/// Returns 404 when the task does not exist, 422 when the task has not yet
/// reached a terminal state.
pub(crate) async fn get_task_proof(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Response {
    let id = harness_core::types::TaskId(task_id);

    let task = match state.core.tasks.get_with_db_fallback(&id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("get_task_proof: DB lookup failed for {id:?}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    if !matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "proof-of-work is only available for terminal tasks",
                "status": task.status.as_ref(),
            })),
        )
            .into_response();
    }

    let proof = proof_from_state(&task);
    Json(proof).into_response()
}

#[cfg(test)]
#[path = "proof_handler_tests.rs"]
mod tests;
