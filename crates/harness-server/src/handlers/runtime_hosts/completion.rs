use super::*;
use crate::http::rest_contract::LegacyJson as Json;
use harness_workflow::runtime::store::runtime_job_leases::{
    RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRequest,
};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

const COMPLETION_EVIDENCE_TIMEOUT_SECS: u64 = crate::runtime_hosts::MAX_LEASE_SECS as u64 - 30;

pub async fn complete_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path((host_id, runtime_job_id)): Path<(String, String)>,
    Json(req): Json<CompleteRuntimeJobRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _runtime_job_operation = state
        .runtime_hosts
        .lock_runtime_job_operation(&runtime_job_id)
        .await;
    let host_operation = state.runtime_hosts.lock_operation(&host_id).await;
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to load runtime job" })),
            );
        }
    };
    let CompleteRuntimeJobRequest {
        lease_expires_at,
        lease_generation,
        result,
    } = req;
    let result = crate::workflow_runtime_worker::strip_caller_transcript_unavailable_signal(result);
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
    let (completion_lease_expires_at, completion_lease_generation) = match reserve_completion_lease(
        store.as_ref(),
        &job,
        &host_id,
        lease_expires_at,
        lease_generation,
    )
    .await
    {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    // The database lease fence, not the host-wide lifecycle lock, owns the
    // completion from this point forward. GitHub verification may block for
    // minutes; keeping this lock would also block heartbeats and unrelated job
    // renewals for the same host. Deregistration can safely revoke the reserved
    // lease, and the fenced completion commit below will then fail closed.
    drop(host_operation);
    let result = match tokio::time::timeout(
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
                reserved_lease_error_response(
                    format!("failed to verify runtime completion evidence: {error}"),
                    completion_lease_expires_at,
                    completion_lease_generation,
                ),
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
                reserved_lease_error_response(
                    "runtime completion evidence verification timed out",
                    completion_lease_expires_at,
                    completion_lease_generation,
                ),
            );
        }
    };
    let result_payload = match serde_json::to_value(&result) {
        Ok(value) => value,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                reserved_lease_error_response(
                    format!("invalid activity result: {e}"),
                    completion_lease_expires_at,
                    completion_lease_generation,
                ),
            );
        }
    };

    let completion = match store
        .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &runtime_job_id,
            &host_id,
            completion_lease_expires_at,
            Some(completion_lease_generation),
            &result,
            transcript.as_ref(),
        )
        .await
    {
        Ok(Some(completion)) => completion,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "completed": false,
                    "error": "runtime job lease is not owned by this host"
                })),
            );
        }
        Err(e) if e.downcast_ref::<RuntimeJobNotFoundError>().is_some() => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": e.to_string() })),
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
                reserved_lease_error_response(
                    format!("failed to complete runtime job: {e}"),
                    completion_lease_expires_at,
                    completion_lease_generation,
                ),
            );
        }
    };

    if let Err(e) = store
        .record_runtime_event(&runtime_job_id, "ActivityResultReady", result_payload)
        .await
    {
        tracing::warn!(
            runtime_job_id = %runtime_job_id,
            error = %e,
            "runtime host completion succeeded but runtime event recording failed"
        );
    }

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
        Json(json!({
            "completed": true,
            "runtime_job": runtime_job,
            "workflow_event": completion.workflow_event,
            "decision": completion.decision,
        })),
    )
}

pub(super) fn reserved_lease_error_response(
    error: impl Into<String>,
    lease_expires_at: DateTime<Utc>,
    lease_generation: u64,
) -> Json<serde_json::Value> {
    Json(json!({
        "error": error.into(),
        "lease_expires_at": lease_expires_at,
        "lease_generation": lease_generation,
        "lease_reserved": true,
    }))
}

pub(super) fn eval_resource_limit_preflight_failure(
    job: &RuntimeJob,
    error: &str,
    resource_limits: Option<CappedResourceLimits>,
) -> ActivityResult {
    let mut artifact = json!({
        "enforced": false,
        "reason": error,
    });
    if let Some(resource_limits) = resource_limits {
        artifact["resource_limits"] = json!(resource_limits);
    }
    ActivityResult::failed(
        runtime_job_activity(job),
        "Evaluation resource limits could not be enforced.",
        error,
    )
    .with_error_kind(ActivityErrorKind::Configuration)
    .with_artifact(ActivityArtifact::new(
        "resource_limit_enforcement",
        artifact,
    ))
}

fn runtime_job_activity(job: &RuntimeJob) -> String {
    job.input
        .get("activity")
        .and_then(Value::as_str)
        .or_else(|| {
            job.input
                .pointer("/command/activity")
                .and_then(Value::as_str)
        })
        .unwrap_or("remote_host")
        .to_string()
}

pub(super) fn validate_eval_resource_limit_report(
    job: &RuntimeJob,
    result: &ActivityResult,
) -> Result<(), (StatusCode, serde_json::Value)> {
    let Some(expected_limits) = eval_resource_limit_enforcement_for_job(job).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid eval resource limits: {error}") }),
        )
    })?
    else {
        return Ok(());
    };

    let Some(report_value) = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "resource_limit_report")
        .map(|artifact| artifact.artifact.clone())
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "eval runtime job completion requires resource_limit_report artifact"
            }),
        ));
    };
    let report: ResourceLimitReport = serde_json::from_value(report_value).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid resource_limit_report artifact: {error}") }),
        )
    })?;
    if report.limits.effective != expected_limits.effective {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report limits do not match claimed eval resource limits"
            }),
        ));
    }
    if !resource_usage_has_evidence(&report.usage) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report requires usage evidence"
            }),
        ));
    }
    if report.reason.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report requires a non-empty reason"
            }),
        ));
    }
    Ok(())
}

fn resource_usage_has_evidence(usage: &harness_sandbox::ResourceUsage) -> bool {
    usage.cpu_time_millis.is_some()
        || usage.peak_memory_bytes.is_some()
        || usage.peak_pids.is_some()
        || usage.disk_bytes.is_some()
        || usage.output_bytes.is_some()
        || usage.wall_time_millis.is_some()
}

async fn reserve_completion_lease(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
    owner: &str,
    previous_expires_at: DateTime<Utc>,
    lease_generation: Option<u64>,
) -> Result<(DateTime<Utc>, u64), (StatusCode, Json<serde_json::Value>)> {
    let lease_generation = lease_generation.unwrap_or(job.lease_generation);
    let renewal_id =
        completion_reservation_id(&job.id, owner, lease_generation, previous_expires_at);
    let now = Utc::now();
    let renew = || {
        store.renew_remote_host_runtime_job_lease(RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &job.id,
            owner,
            lease_generation,
            previous_expires_at,
            renewal_id,
            lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            now,
            max_lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            owner_active: true,
        })
    };
    let outcome = match renew().await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            tracing::warn!(
                runtime_job_id = %job.id,
                owner,
                %renewal_id,
                %error,
                "runtime host completion lease reservation returned an ambiguous store error; reconciling"
            );
            renew().await
        }
    };
    match outcome {
        Ok(RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_generation,
            lease_expires_at,
            ..
        }) => Ok((lease_expires_at, lease_generation)),
        Ok(RuntimeJobLeaseRenewalOutcome::LeaseLost { .. }) => Err(lease::lease_lost_response()),
        Ok(RuntimeJobLeaseRenewalOutcome::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "runtime job not found" })),
        )),
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                owner,
                %error,
                "runtime host failed to reserve completion lease"
            );
            Err(lease::workflow_store_unavailable_response())
        }
    }
}

pub(super) fn completion_reservation_id(
    runtime_job_id: &str,
    owner: &str,
    lease_generation: u64,
    previous_expires_at: DateTime<Utc>,
) -> Uuid {
    let mut digest = Sha256::new();
    for component in [
        runtime_job_id.as_bytes(),
        owner.as_bytes(),
        &lease_generation.to_be_bytes(),
        previous_expires_at.to_rfc3339().as_bytes(),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component);
    }
    let digest = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

pub(super) async fn replay_completion_reservation(
    store: &WorkflowRuntimeStore,
    runtime_job_id: &str,
    owner: &str,
    lease_generation: u64,
    previous_expires_at: DateTime<Utc>,
) -> anyhow::Result<RuntimeJobLeaseRenewalOutcome> {
    store
        .renew_remote_host_runtime_job_lease(RuntimeJobLeaseRenewalRequest {
            runtime_job_id,
            owner,
            lease_generation,
            previous_expires_at,
            renewal_id: completion_reservation_id(
                runtime_job_id,
                owner,
                lease_generation,
                previous_expires_at,
            ),
            lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            now: Utc::now(),
            max_lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            owner_active: true,
        })
        .await
}
