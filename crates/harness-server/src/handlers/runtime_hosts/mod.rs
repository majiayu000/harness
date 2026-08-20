mod completion;
pub use completion::complete_runtime_job_for_runtime_host;
#[cfg(test)]
pub(crate) use completion::replay_completion_reservation;
mod claim;
use claim::defer_runtime_host_resource_limit_claim;

use crate::http::rest_contract::{ContractJson, LegacyJson as Json, PrimitivePath as Path};
use crate::http::AppState;
use axum::{
    extract::{rejection::JsonRejection, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use harness_protocol::rest::{
    ClaimRuntimeJobRequest, RuntimeHostClaimResponse, RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY,
};
use harness_sandbox::{
    CappedResourceLimits, ResourceLimitReport, ResourceLimits, EVAL_RESOURCE_LIMITS_CAPABILITY,
};
use harness_workflow::runtime::{
    prepare_runtime_transcript, ActivityArtifact, ActivityErrorKind, ActivityResult, RuntimeJob,
    RuntimeJobClaimDecision, RuntimeJobClaimDeferOutcome, RuntimeJobClaimGuard,
    RuntimeJobCompletionLease, RuntimeJobNotFoundError, WorkflowRuntimeStore,
    TRUSTED_EVAL_VERIFIER_V1_CAPABILITY,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) mod lease;
pub use lease::renew_runtime_job_lease_for_runtime_host;

type ClaimJson = ContractJson<RuntimeHostClaimResponse>;

fn claim_json(value: serde_json::Value) -> ClaimJson {
    ContractJson(RuntimeHostClaimResponse(value))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRuntimeHostRequest {
    pub host_id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

pub use harness_protocol::rest::{CompleteRuntimeJobRequest, RuntimeHostExecutionEvidence};

pub async fn list_runtime_hosts(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let hosts = state.runtime_hosts.list_hosts();
    (StatusCode::OK, Json(json!({ "hosts": hosts })))
}

pub(crate) async fn active_runtime_job_lease_counts(
    state: &AppState,
) -> anyhow::Result<BTreeMap<String, u64>> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("workflow runtime store unavailable"))?;
    store
        .count_remote_host_runtime_job_leases_by_owner()
        .await?
        .into_iter()
        .map(|(host_id, count)| {
            u64::try_from(count)
                .map(|count| (host_id, count))
                .map_err(|_| anyhow::anyhow!("runtime job lease count is negative"))
        })
        .collect()
}

pub(crate) async fn active_runtime_job_lease_count_total(state: &AppState) -> anyhow::Result<u64> {
    let counts = active_runtime_job_lease_counts(state).await?;
    Ok(state
        .runtime_hosts
        .list_hosts()
        .iter()
        .filter_map(|host| counts.get(&host.id))
        .copied()
        .fold(0_u64, u64::saturating_add))
}

pub async fn register_runtime_host(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterRuntimeHostRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let host_id = req.host_id.trim();
    if host_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "host_id must not be empty" })),
        );
    }
    if let Err(response) = ensure_runtime_state_persistence_available(&state) {
        return response;
    }
    let _host_operation = state.runtime_hosts.lock_operation(host_id).await;
    let host = state.runtime_hosts.register(
        host_id.to_string(),
        req.display_name.map(|v| v.trim().to_string()),
        req.capabilities,
    );
    if let Err(response) = persist_runtime_state(&state).await {
        return response;
    }
    (StatusCode::OK, Json(json!({ "host": host })))
}

pub async fn heartbeat_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    match state.runtime_hosts.heartbeat(&host_id) {
        Ok(host) => {
            // Heartbeat is intentionally not persisted (transient data, self-healing
            // after restart).  However if a prior mutation left the dirty flag, piggyback
            // on this frequent call to converge durable state.
            if state.is_runtime_state_dirty() {
                if let Err(e) = state.persist_runtime_state().await {
                    tracing::warn!(
                        host_id = %host_id,
                        error = %e,
                        "opportunistic dirty-state flush on heartbeat failed; will retry next heartbeat"
                    );
                }
            }
            (StatusCode::OK, Json(json!({ "host": host })))
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

pub async fn deregister_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = ensure_runtime_state_persistence_available(&state) {
        return response;
    }
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        // Host already gone from memory (idempotent retry).  If a prior
        // deregister mutated memory but failed to persist, converge now.
        if state.is_runtime_state_dirty() {
            if let Err(response) = persist_runtime_state(&state).await {
                return response;
            }
        }
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "runtime host not found" })),
        )
    } else {
        let Some(previous_lifecycle) = state.runtime_hosts.mark_draining(&host_id) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "runtime host not found" })),
            );
        };
        if let Err(response) = persist_runtime_state(&state).await {
            state
                .runtime_hosts
                .set_lifecycle(&host_id, previous_lifecycle);
            return response;
        }

        let store = match workflow_runtime_store(&state) {
            Ok(store) => store,
            Err(response) => return response,
        };
        if let Err(error) = store
            .revoke_remote_host_runtime_job_leases(&host_id, Utc::now())
            .await
        {
            tracing::error!(
                host_id = %host_id,
                error = %error,
                "failed to revoke workflow runtime-job leases during deregistration"
            );
            return crate::http::api_error::ApiError::store_unavailable("workflow runtime store")
                .into_status_json();
        }
        match store.count_remote_host_runtime_job_leases(&host_id).await {
            Ok(0) => {}
            Ok(remaining) => {
                tracing::error!(
                    host_id = %host_id,
                    remaining_leases = remaining,
                    "runtime host remains draining because workflow leases remain owned"
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "runtime host lease revocation incomplete" })),
                );
            }
            Err(error) => {
                tracing::error!(
                    host_id = %host_id,
                    error = %error,
                    "failed to confirm workflow runtime-job revocation"
                );
                return crate::http::api_error::ApiError::store_unavailable(
                    "workflow runtime store",
                )
                .into_status_json();
            }
        }

        let draining_record = state
            .runtime_hosts
            .hosts
            .get(&host_id)
            .map(|host| host.clone());
        if !state.runtime_hosts.deregister(&host_id) {
            tracing::warn!(
                host_id = %host_id,
                "runtime host disappeared during deregistration after task lease release"
            );
        }
        state.runtime_project_cache.clear_host(&host_id);
        if let Err(response) = persist_runtime_state(&state).await {
            if let Some(record) = draining_record {
                state.runtime_hosts.hosts.insert(host_id.clone(), record);
            }
            return response;
        }
        (StatusCode::OK, Json(json!({ "deregistered": true })))
    }
}

pub async fn claim_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
    payload: Result<ContractJson<ClaimRuntimeJobRequest>, JsonRejection>,
) -> (StatusCode, ClaimJson) {
    let ContractJson(req) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                claim_json(json!({ "error": "invalid runtime job claim request" })),
            )
        }
    };
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            claim_json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
    }
    if !state.runtime_hosts.is_active(&host_id) {
        return (
            StatusCode::CONFLICT,
            claim_json(json!({ "error": "runtime host is draining" })),
        );
    }
    if !runtime_host_supports_capability(&state, &host_id, RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY) {
        return (
            StatusCode::OK,
            claim_json(json!({
                "claimed": false,
                "upgrade_required": true,
                "required_capability": RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY,
            })),
        );
    }
    let host_supports_eval_resource_limits =
        runtime_host_supports_eval_resource_limits(&state, &host_id);
    let host_supports_trusted_eval_verifier =
        runtime_host_supports_capability(&state, &host_id, TRUSTED_EVAL_VERIFIER_V1_CAPABILITY);
    let store = match workflow_runtime_store(&state) {
        Ok(store) => store,
        Err(response) => return (response.0, claim_json(response.1 .0)),
    };
    if let Some(project) = req.project.as_deref().map(str::trim) {
        if !project.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                claim_json(json!({
                    "error": "project filtering is not supported for workflow runtime-job claims"
                })),
            );
        }
    }
    let lease_expires_at = match lease::runtime_host_lease_expires_at(req.lease_secs.value()) {
        Ok(value) => value,
        Err(response) => return (response.0, claim_json(response.1 .0)),
    };

    let mut job = match store
        .claim_next_remote_host_runtime_job(
            &host_id,
            lease_expires_at,
            host_supports_eval_resource_limits,
            host_supports_trusted_eval_verifier,
        )
        .await
    {
        Ok(Some(job)) => job,
        Ok(None) => return (StatusCode::OK, claim_json(json!({ "claimed": false }))),
        Err(e) => {
            tracing::error!(
                host_id = %host_id,
                error = %e,
                "runtime host failed to claim workflow runtime job"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                claim_json(json!({ "error": format!("failed to claim runtime job: {e}") })),
            );
        }
    };
    let lease_proof = match lease::required_remote_runtime_job_lease_proof(
        store.as_ref(),
        &job.id,
        &host_id,
        job.lease_generation,
        lease_expires_at,
    )
    .await
    {
        Some(proof) => proof,
        None => {
            let response = lease::workflow_store_unavailable_response();
            return (response.0, claim_json(response.1 .0));
        }
    };

    match state
        .runtime_circuit_breakers
        .before_execute(&job, Utc::now(), lease_expires_at)
    {
        RuntimeJobClaimDecision::Proceed => {}
        RuntimeJobClaimDecision::Defer { not_before, reason } => {
            match store
                .defer_runtime_job_claim_if_owned(&job.id, &host_id, lease_expires_at, not_before)
                .await
            {
                Ok(RuntimeJobClaimDeferOutcome::Deferred(_)) => {
                    if let Err(e) = store
                        .record_runtime_event(
                            &job.id,
                            "RuntimeJobClaimDeferred",
                            json!({
                                "owner": host_id.as_str(),
                                "not_before": not_before,
                                "reason": reason,
                                "claim_api": "runtime_host",
                            }),
                        )
                        .await
                    {
                        tracing::warn!(
                            runtime_job_id = %job.id,
                            error = %e,
                            "runtime host claim defer succeeded but runtime event recording failed"
                        );
                    }
                    return (StatusCode::OK, claim_json(json!({ "claimed": false })));
                }
                Ok(RuntimeJobClaimDeferOutcome::CancellationRequested(cancelled)) => {
                    return (
                        StatusCode::OK,
                        claim_json(runtime_host_claim(cancelled, lease_expires_at, lease_proof)),
                    );
                }
                Ok(RuntimeJobClaimDeferOutcome::StaleLease) => {
                    tracing::warn!(
                        runtime_job_id = %job.id,
                        host_id = %host_id,
                        "runtime host claim defer ignored because the host no longer owns the lease"
                    );
                    return (StatusCode::OK, claim_json(json!({ "claimed": false })));
                }
                Err(e) => {
                    tracing::error!(
                        runtime_job_id = %job.id,
                        host_id = %host_id,
                        error = %e,
                        "runtime host failed to defer circuit-breaker-blocked runtime job"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        claim_json(json!({ "error": format!("failed to defer runtime job: {e}") })),
                    );
                }
            }
        }
    }

    let resource_limits = match eval_resource_limit_enforcement_for_job(&job) {
        Ok(Some(resource_limits)) => {
            if !host_supports_eval_resource_limits {
                let (status, response) = defer_runtime_host_resource_limit_claim(
                    store.as_ref(),
                    &host_id,
                    lease_expires_at,
                    &job,
                    "runtime host lacks eval_resource_limits capability",
                )
                .await;
                return (status, claim_json(response));
            }
            Some(resource_limits)
        }
        Ok(None) => None,
        Err(error) => {
            let result = completion::eval_resource_limit_preflight_failure(&job, &error, None);
            return complete_runtime_host_preflight_failure(
                &state,
                store.as_ref(),
                &host_id,
                lease_expires_at,
                &job,
                result,
            )
            .await;
        }
    };

    if let Some(resource_limits) = &resource_limits {
        set_eval_resource_limit_enforcement(&mut job, resource_limits);
        if let Err(error) = store
            .record_runtime_event(
                &job.id,
                "EvalResourceLimitsApplied",
                json!({
                    "host_id": host_id.as_str(),
                    "resource_limits": resource_limits,
                    "reason": "runtime host claim",
                }),
            )
            .await
        {
            tracing::warn!(
                runtime_job_id = %job.id,
                host_id = %host_id,
                %error,
                "runtime host claim succeeded but eval resource-limit event recording failed"
            );
        }
    }

    if let Err(result) =
        crate::workflow_runtime_worker::hydrate_exact_replay_transcript(&state, &mut job).await
    {
        return complete_runtime_host_preflight_failure(
            &state,
            store.as_ref(),
            &host_id,
            lease_expires_at,
            &job,
            result,
        )
        .await;
    }

    let credential_environment =
        match crate::eval_credentials::attach_runtime_host_eval_environment_policy(&mut job) {
            Ok(environment) => environment,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    claim_json(
                        json!({ "error": format!("invalid eval credential environment: {error}") }),
                    ),
                );
            }
        };
    if let Some(credential_environment) = credential_environment.as_ref() {
        match store.get_command(&job.command_id).await {
            Ok(Some(command)) => {
                if let Err(error) = store
                    .append_event(
                        &command.workflow_id,
                        "RuntimeHostEvalCredentialPolicyIssued",
                        "runtime_host_claim",
                        json!({
                            "runtime_job_id": job.id.clone(),
                            "host_id": host_id.clone(),
                            "credential_environment": credential_environment.audit(),
                        }),
                    )
                    .await
                {
                    tracing::error!(
                        runtime_job_id = %job.id,
                        host_id = %host_id,
                        error = %error,
                        "failed to persist remote eval credential policy audit"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        claim_json(
                            json!({ "error": "failed to persist eval credential policy audit" }),
                        ),
                    );
                }
            }
            Ok(None) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    claim_json(json!({ "error": "runtime job command missing during claim" })),
                );
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    claim_json(
                        json!({ "error": format!("failed to load runtime job command: {error}") }),
                    ),
                );
            }
        }
    }
    let mut response = runtime_host_claim(job, lease_expires_at, lease_proof);
    if let Some(credential_environment) = credential_environment {
        response["credential_environment"] = json!(credential_environment.audit());
        response["credential_environment_variables"] = json!(credential_environment.variables());
    }
    if let Some(resource_limits) = resource_limits {
        response["resource_limits"] = json!(resource_limits);
    }
    (StatusCode::OK, claim_json(response))
}

pub(super) fn runtime_host_claim(
    job: RuntimeJob,
    lease_expires_at: DateTime<Utc>,
    lease_proof: uuid::Uuid,
) -> Value {
    json!({
        "claimed": true,
        "runtime_job_id": job.id,
        "lease_generation": job.lease_generation,
        "lease_expires_at": lease_expires_at,
        "lease_proof": lease_proof,
        "runtime_job": job,
    })
}

async fn complete_runtime_host_preflight_failure(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    host_id: &str,
    lease_expires_at: DateTime<Utc>,
    job: &RuntimeJob,
    result: ActivityResult,
) -> (StatusCode, ClaimJson) {
    let lease_proof = match lease::required_remote_runtime_job_lease_proof(
        store,
        &job.id,
        host_id,
        job.lease_generation,
        lease_expires_at,
    )
    .await
    {
        Some(proof) => proof,
        None => {
            let response = lease::workflow_store_unavailable_response();
            return (response.0, claim_json(response.1 .0));
        }
    };
    let completion = match store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &job.id,
            RuntimeJobCompletionLease::remote(
                host_id,
                lease_expires_at,
                job.lease_generation,
                Some(lease_proof),
            ),
            &result,
        )
        .await
    {
        Ok(Some(completion)) => completion,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                claim_json(json!({
                    "claimed": false,
                    "error": "runtime job lease is not owned by this host"
                })),
            );
        }
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                %error,
                "failed to persist remote runtime job preflight failure"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                claim_json(
                    json!({ "error": format!("failed to complete runtime job preflight: {error}") }),
                ),
            );
        }
    };
    let mut runtime_job = completion.runtime_job;
    if let Err(error) = crate::workflow_runtime_worker::record_runtime_circuit_breaker_completion(
        state.as_ref(),
        store,
        &mut runtime_job,
    )
    .await
    {
        tracing::warn!(
            runtime_job_id = %job.id,
            %error,
            "runtime job preflight completion succeeded but circuit breaker update failed"
        );
    }
    (
        StatusCode::OK,
        claim_json(json!({
            "claimed": false,
            "preflight_failed": true,
            "runtime_job_id": job.id,
            "runtime_job": runtime_job,
            "workflow_event": completion.workflow_event,
            "decision": completion.decision,
        })),
    )
}

fn runtime_host_supports_eval_resource_limits(state: &Arc<AppState>, host_id: &str) -> bool {
    runtime_host_supports_capability(state, host_id, EVAL_RESOURCE_LIMITS_CAPABILITY)
}

fn runtime_host_supports_capability(state: &Arc<AppState>, host_id: &str, required: &str) -> bool {
    state.runtime_hosts.hosts.get(host_id).is_some_and(|host| {
        host.capabilities
            .iter()
            .any(|capability| capability == required)
    })
}

fn eval_resource_limit_enforcement_for_job(
    job: &RuntimeJob,
) -> Result<Option<CappedResourceLimits>, String> {
    let Some(eval) = eval_metadata(&job.input) else {
        return Ok(None);
    };
    if let Some(value) = eval.get("resource_limits") {
        if value.get("requested").is_some() && value.get("effective").is_some() {
            return serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| format!("invalid eval resource_limits: {error}"));
        }
        let requested: ResourceLimits = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid eval resource_limits: {error}"))?;
        return requested
            .cap_by(ResourceLimits::operator_default_maxima())
            .map(Some)
            .map_err(|error| format!("invalid eval resource_limits: {error}"));
    }
    let timeout_secs = eval
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            "eval runtime job must include timeout_secs or resource_limits".to_string()
        })?;
    ResourceLimits::evaluation_defaults(timeout_secs)
        .cap_by(ResourceLimits::operator_default_maxima())
        .map(Some)
        .map_err(|error| format!("invalid eval resource_limits: {error}"))
}

fn eval_metadata(input: &Value) -> Option<&Value> {
    input
        .pointer("/command/eval")
        .or_else(|| input.get("eval"))
        .filter(|value| value.is_object())
}

fn set_eval_resource_limit_enforcement(
    job: &mut RuntimeJob,
    resource_limits: &CappedResourceLimits,
) {
    let value = json!(resource_limits);
    if let Some(eval) = job
        .input
        .pointer_mut("/command/eval")
        .and_then(Value::as_object_mut)
    {
        eval.insert("resource_limits".to_string(), value.clone());
    }
    if let Some(eval) = job.input.get_mut("eval").and_then(Value::as_object_mut) {
        eval.insert("resource_limits".to_string(), value);
    }
}

fn ensure_runtime_state_persistence_available(
    state: &Arc<AppState>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Err(e) = state.ensure_runtime_state_persistence_available() {
        tracing::error!(
            "runtime host mutation rejected because runtime state persistence is unavailable: {e}"
        );
        return Err(runtime_state_persistence_error_response(e));
    }
    Ok(())
}

async fn persist_runtime_state(
    state: &Arc<AppState>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Err(e) = state.persist_runtime_state().await {
        tracing::error!("failed to persist runtime state after runtime host mutation: {e}");
        return Err(runtime_state_persistence_error_response(e));
    }
    Ok(())
}

fn workflow_runtime_store(
    state: &Arc<AppState>,
) -> Result<Arc<WorkflowRuntimeStore>, (StatusCode, Json<serde_json::Value>)> {
    state
        .workflow_runtime_store()
        .cloned()
        .map_err(|error| error.into_status_json())
}

fn runtime_state_persistence_error_response(
    error: anyhow::Error,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "runtime state persistence unavailable",
            "message": error.to_string(),
        })),
    )
}
