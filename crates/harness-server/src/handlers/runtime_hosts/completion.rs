use super::*;
use crate::http::rest_contract::ContractJson;
use harness_protocol::rest::RuntimeHostCompletionResponse;
use harness_workflow::runtime::store::RemoteStaleCompletionOutcome;
use std::time::Duration;

const COMPLETION_EVIDENCE_TIMEOUT_SECS: u64 = crate::runtime_hosts::MAX_LEASE_SECS as u64 - 30;

type CompletionJson = ContractJson<RuntimeHostCompletionResponse>;

fn completion_json(value: serde_json::Value) -> CompletionJson {
    ContractJson(RuntimeHostCompletionResponse(value))
}

pub async fn complete_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path((host_id, runtime_job_id)): Path<(String, String)>,
    ContractJson(req): ContractJson<CompleteRuntimeJobRequest>,
) -> (StatusCode, CompletionJson) {
    let _runtime_job_operation = state
        .runtime_hosts
        .lock_runtime_job_operation(&runtime_job_id)
        .await;
    let host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    if state.runtime_hosts.lifecycle(&host_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            completion_json(
                json!({ "error": format!("runtime host '{host_id}' is not registered") }),
            ),
        );
    }
    let store = match workflow_runtime_store(&state) {
        Ok(store) => store,
        Err((status, body)) => return (status, completion_json(body.0)),
    };
    let job = match store.get_runtime_job(&runtime_job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                completion_json(
                    json!({ "error": format!("runtime job not found: {runtime_job_id}") }),
                ),
            );
        }
        Err(error) => {
            tracing::error!(
                host_id = %host_id,
                runtime_job_id = %runtime_job_id,
                %error,
                "runtime host failed to load workflow runtime job before completion"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                completion_json(json!({ "error": "failed to load runtime job" })),
            );
        }
    };
    let CompleteRuntimeJobRequest {
        lease_expires_at,
        lease_generation,
        lease_proof,
        result: result_value,
        execution_evidence,
    } = req;
    let lease_generation = match lease_generation {
        Some(lease_generation) => lease_generation,
        None if lease_proof.is_none() => match store
            .remote_legacy_runtime_job_lease_generation(&runtime_job_id, &host_id, lease_expires_at)
            .await
        {
            Ok(Some(lease_generation)) => lease_generation,
            Ok(None) => {
                let (status, body) = lease::lease_lost_response();
                return (status, completion_json(body.0));
            }
            Err(error) => {
                tracing::error!(
                    host_id = %host_id,
                    runtime_job_id = %runtime_job_id,
                    %error,
                    "legacy runtime completion lease generation lookup failed"
                );
                let (status, body) = lease::workflow_store_unavailable_response();
                return (status, completion_json(body.0));
            }
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                completion_json(
                    json!({ "error": "lease_generation is required with lease_proof" }),
                ),
            );
        }
    };
    let result: ActivityResult = match serde_json::from_value(result_value) {
        Ok(result) => result,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                completion_json(json!({ "error": format!("invalid activity result: {error}") })),
            );
        }
    };
    let result = crate::workflow_runtime_worker::strip_caller_transcript_unavailable_signal(result);
    let cancellation_ack = is_eval_cancellation_ack(&job, &result);
    if !cancellation_ack {
        if let Err(response) = validate_eval_host_capabilities(&state, &host_id, &job) {
            return (StatusCode::BAD_REQUEST, completion_json(response));
        }
    }
    let result = match if cancellation_ack {
        attach_eval_cancellation_cleanup_evidence(result, execution_evidence)
    } else {
        attach_eval_checkout_evidence(&job, result, execution_evidence)
    } {
        Ok(result) => result,
        Err(response) => return (StatusCode::BAD_REQUEST, completion_json(response)),
    };
    if !cancellation_ack {
        if let Err((status, response)) = validate_eval_resource_limit_report(&job, &result) {
            return (status, completion_json(response));
        }
    }
    let (result, transcript) = match prepare_runtime_transcript(&job, result) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                completion_json(
                    json!({ "error": format!("invalid runtime transcript source: {error}") }),
                ),
            );
        }
    };
    let completion_reservation = match reserve_completion_lease(
        store.as_ref(),
        &job,
        &host_id,
        lease_expires_at,
        Some(lease_generation),
        lease_proof,
        cancellation_ack,
    )
    .await
    {
        Ok(reservation) => reservation,
        Err(response) => return response,
    };
    // The database lease fence, not the host-wide lifecycle lock, owns the
    // completion from this point forward. GitHub verification may block for
    // minutes; keeping this lock would also block heartbeats and unrelated job
    // renewals for the same host. Deregistration can safely revoke the reserved
    // lease, and the fenced completion commit below will then fail closed.
    drop(host_operation);
    let result = if cancellation_ack {
        result
    } else {
        match tokio::time::timeout(
            Duration::from_secs(COMPLETION_EVIDENCE_TIMEOUT_SECS),
            crate::workflow_runtime_worker::remote_completion::apply_remote_completion_evidence(
                &state, &job, result,
            ),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::error!(
                    host_id = %host_id,
                    runtime_job_id = %runtime_job_id,
                    %error,
                    "runtime host completion evidence verification failed"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    completion_reservation.error_response(format!(
                        "failed to verify runtime completion evidence: {error}"
                    )),
                );
            }
            Err(_) => {
                tracing::error!(
                    host_id = %host_id,
                    runtime_job_id = %runtime_job_id,
                    "runtime host completion evidence verification timed out"
                );
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    completion_reservation
                        .error_response("runtime completion evidence verification timed out"),
                );
            }
        }
    };
    let completion_lease = completion_reservation.completion_lease(&host_id);
    if completion_reservation.is_issued_but_stale() {
        let stale_outcome = match store
            .record_remote_stale_completion_if_issued(
                &runtime_job_id,
                completion_lease,
                &result,
                transcript.as_ref(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(
                    host_id = %host_id,
                    runtime_job_id = %runtime_job_id,
                    %error,
                    "runtime host failed to dead-letter stale completion"
                );
                RemoteStaleCompletionOutcome::Rejected
            }
        };
        if stale_outcome == RemoteStaleCompletionOutcome::CancellationRequested {
            return (
                StatusCode::CONFLICT,
                completion_json(lease::cancellation_requested_body()),
            );
        }
        return (
            StatusCode::CONFLICT,
            completion_json(json!({
                "completed": false,
                "error": "runtime job lease is not owned by this host",
                "dead_lettered": stale_outcome == RemoteStaleCompletionOutcome::DeadLettered,
            })),
        );
    }

    let completion_result = if cancellation_ack {
        store
            .commit_cancelled_runtime_activity_completion_with_transcript_if_owned_with_generation(
                &runtime_job_id,
                completion_lease,
                &result,
                transcript.as_ref(),
            )
            .await
    } else {
        store
            .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
                &runtime_job_id,
                completion_lease,
                &result,
                transcript.as_ref(),
            )
            .await
    };
    let completion = match completion_result {
        Ok(Some(completion)) => completion,
        Ok(None) => {
            let stale_outcome = match store
                .record_remote_stale_completion_if_issued(
                    &runtime_job_id,
                    completion_lease,
                    &result,
                    transcript.as_ref(),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::error!(
                        host_id = %host_id,
                        runtime_job_id = %runtime_job_id,
                        %error,
                        "runtime host failed to dead-letter stale completion"
                    );
                    RemoteStaleCompletionOutcome::Rejected
                }
            };
            if stale_outcome == RemoteStaleCompletionOutcome::CancellationRequested {
                return (
                    StatusCode::CONFLICT,
                    completion_json(lease::cancellation_requested_body()),
                );
            }
            return (
                StatusCode::CONFLICT,
                completion_json(json!({
                    "completed": false,
                    "error": "runtime job lease is not owned by this host",
                    "dead_lettered": stale_outcome == RemoteStaleCompletionOutcome::DeadLettered,
                })),
            );
        }
        Err(e) if e.downcast_ref::<RuntimeJobNotFoundError>().is_some() => {
            return (
                StatusCode::NOT_FOUND,
                completion_json(json!({ "error": e.to_string() })),
            );
        }
        Err(e) => {
            tracing::error!(
                host_id = %host_id,
                runtime_job_id = %runtime_job_id,
                error = %e,
                "runtime host failed to complete workflow runtime job"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                completion_reservation
                    .error_response(format!("failed to complete runtime job: {e}")),
            );
        }
    };

    let mut runtime_job = completion.runtime_job;
    if let Err(e) = crate::workflow_runtime_worker::record_runtime_circuit_breaker_completion(
        state.as_ref(),
        store.as_ref(),
        &mut runtime_job,
    )
    .await
    {
        tracing::warn!(
            runtime_job_id = %runtime_job_id,
            error = %e,
            "runtime host completion succeeded but circuit breaker update failed"
        );
    }

    (
        StatusCode::OK,
        completion_json(json!({
            "completed": true,
            "runtime_job": runtime_job,
            "workflow_event": completion.workflow_event,
            "decision": completion.decision,
        })),
    )
}

mod capabilities;
mod evidence;
mod reservation;
use capabilities::validate_eval_host_capabilities;
use evidence::{
    attach_eval_cancellation_cleanup_evidence, attach_eval_checkout_evidence,
    is_eval_cancellation_ack,
};
pub(super) use evidence::{
    eval_resource_limit_preflight_failure, validate_eval_resource_limit_report,
};
pub(crate) use reservation::replay_completion_reservation;
use reservation::reserve_completion_lease;
