use super::{lease, validate_eval_resource_limit_report, workflow_runtime_store};
use crate::http::rest_contract::{LegacyJson as Json, PrimitivePath as Path};
use crate::http::AppState;
use axum::{extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use harness_workflow::runtime::{
    prepare_runtime_transcript, ActivityResult, RuntimeJobCompletionLease, RuntimeJobNotFoundError,
    RuntimeKind,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct CompleteRuntimeJobRequest {
    pub lease_expires_at: DateTime<Utc>,
    #[serde(default)]
    pub lease_generation: Option<u64>,
    #[serde(default)]
    pub lease_proof: Option<Uuid>,
    pub result: ActivityResult,
}

pub async fn complete_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path((host_id, runtime_job_id)): Path<(String, String)>,
    Json(req): Json<CompleteRuntimeJobRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
    }
    if !state.runtime_hosts.is_active(&host_id) {
        return lease::lease_lost_response();
    }
    let store = match workflow_runtime_store(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    let job = match store.get_runtime_job(&runtime_job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("runtime job not found: {runtime_job_id}") })),
            );
        }
        Err(error) => {
            tracing::error!(
                host_id = %host_id,
                runtime_job_id = %runtime_job_id,
                %error,
                "runtime host failed to load workflow runtime job before completion"
            );
            return lease::workflow_store_unavailable_response();
        }
    };
    if job.runtime_kind != RuntimeKind::RemoteHost {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "runtime job is not assigned to a remote host" })),
        );
    }
    let lease_generation = match req.lease_generation {
        Some(lease_generation) => lease_generation,
        None if req.lease_proof.is_none() => job.lease_generation,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "lease_generation is required with lease_proof" })),
            )
        }
    };
    let result =
        crate::workflow_runtime_worker::strip_caller_transcript_unavailable_signal(req.result);
    if let Err((status, response)) = validate_eval_resource_limit_report(&job, &result) {
        return (status, Json(response));
    }
    let (result, transcript) = match prepare_runtime_transcript(&job, result) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid runtime transcript source: {error}") })),
            );
        }
    };
    let result_payload = match serde_json::to_value(&result) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid activity result: {error}") })),
            );
        }
    };

    let completion = match store
        .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &runtime_job_id,
            RuntimeJobCompletionLease::remote(
                &host_id,
                req.lease_expires_at,
                lease_generation,
                req.lease_proof,
            ),
            &result,
            transcript.as_ref(),
        )
        .await
    {
        Ok(Some(completion)) => completion,
        Ok(None) => {
            let dead_lettered = match store
                .record_remote_stale_completion_if_issued(
                    &runtime_job_id,
                    RuntimeJobCompletionLease::remote(
                        &host_id,
                        req.lease_expires_at,
                        lease_generation,
                        req.lease_proof,
                    ),
                    &result,
                    transcript.as_ref(),
                )
                .await
            {
                Ok(dead_lettered) => dead_lettered,
                Err(error) => {
                    tracing::error!(
                        host_id = %host_id,
                        runtime_job_id = %runtime_job_id,
                        %error,
                        "runtime host stale completion provenance or dead-letter persistence failed"
                    );
                    return lease::workflow_store_unavailable_response();
                }
            };
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "completed": false,
                    "error": "runtime job lease is not owned by this host",
                    "error_code": "lease_lost",
                    "must_stop": true,
                    "dead_lettered": dead_lettered,
                })),
            );
        }
        Err(error) if error.downcast_ref::<RuntimeJobNotFoundError>().is_some() => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": error.to_string() })),
            );
        }
        Err(error) => {
            tracing::error!(
                host_id = %host_id,
                runtime_job_id = %runtime_job_id,
                %error,
                "runtime host failed to complete workflow runtime job"
            );
            return lease::workflow_store_unavailable_response();
        }
    };

    if let Err(error) = store
        .record_runtime_event(&runtime_job_id, "ActivityResultReady", result_payload)
        .await
    {
        tracing::warn!(
            runtime_job_id = %runtime_job_id,
            %error,
            "runtime host completion succeeded but runtime event recording failed"
        );
    }
    let mut runtime_job = completion.runtime_job;
    if let Err(error) = crate::workflow_runtime_worker::record_runtime_circuit_breaker_completion(
        state.as_ref(),
        store.as_ref(),
        &mut runtime_job,
    )
    .await
    {
        tracing::warn!(
            runtime_job_id = %runtime_job_id,
            %error,
            "runtime host completion succeeded but circuit breaker update failed"
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "completed": true,
            "runtime_job": runtime_job,
            "workflow_event": completion.workflow_event,
            "decision": completion.decision,
        })),
    )
}
