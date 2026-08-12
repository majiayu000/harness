use crate::http::rest_contract::{LegacyJson as Json, PrimitivePath as Path};
use crate::http::AppState;
use axum::{
    extract::{rejection::JsonRejection, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use harness_sandbox::{
    CappedResourceLimits, ResourceLimitReport, ResourceLimits, EVAL_RESOURCE_LIMITS_CAPABILITY,
};
use harness_workflow::runtime::{
    prepare_runtime_transcript, ActivityArtifact, ActivityErrorKind, ActivityResult, RuntimeJob,
    RuntimeJobClaimDecision, RuntimeJobClaimGuard, RuntimeJobNotFoundError, RuntimeKind,
    WorkflowRuntimeStore,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) mod lease;
pub use lease::renew_runtime_job_lease_for_runtime_host;

const RESOURCE_LIMIT_CAPABILITY_RETRY_DELAY_SECS: i64 = 30;

#[derive(Debug, Deserialize)]
pub struct RegisterRuntimeHostRequest {
    pub host_id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteRuntimeJobRequest {
    pub lease_expires_at: DateTime<Utc>,
    #[serde(default)]
    pub lease_generation: Option<u64>,
    pub result: ActivityResult,
}

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
    payload: Result<Json<lease::ClaimRuntimeJobRequest>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Json(req) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid runtime job claim request" })),
            )
        }
    };
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
    }
    if !state.runtime_hosts.is_active(&host_id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "runtime host is draining" })),
        );
    }
    let host_supports_eval_resource_limits =
        runtime_host_supports_eval_resource_limits(&state, &host_id);
    let store = match workflow_runtime_store(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    if let Some(project) = req.project.as_deref().map(str::trim) {
        if !project.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "project filtering is not supported for workflow runtime-job claims"
                })),
            );
        }
    }
    let lease_expires_at = match lease::runtime_host_lease_expires_at(req.lease_secs.value()) {
        Ok(value) => value,
        Err(response) => return response,
    };

    let mut job = match store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            &host_id,
            lease_expires_at,
        )
        .await
    {
        Ok(Some(job)) => job,
        Ok(None) => return (StatusCode::OK, Json(json!({ "claimed": false }))),
        Err(e) => {
            tracing::error!(
                host_id = %host_id,
                error = %e,
                "runtime host failed to claim workflow runtime job"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to claim runtime job: {e}") })),
            );
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
                Ok(Some(_)) => {
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
                    return (StatusCode::OK, Json(json!({ "claimed": false })));
                }
                Ok(None) => {
                    tracing::warn!(
                        runtime_job_id = %job.id,
                        host_id = %host_id,
                        "runtime host claim defer ignored because the host no longer owns the lease"
                    );
                    return (StatusCode::OK, Json(json!({ "claimed": false })));
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
                        Json(json!({ "error": format!("failed to defer runtime job: {e}") })),
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
                return (status, Json(response));
            }
            Some(resource_limits)
        }
        Ok(None) => None,
        Err(error) => {
            let result = eval_resource_limit_preflight_failure(&job, &error, None);
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

    let runtime_job_id = job.id.clone();
    let lease_generation = job.lease_generation;
    let mut response = json!({
        "claimed": true,
        "runtime_job": job,
        "runtime_job_id": runtime_job_id,
        "lease_expires_at": lease_expires_at,
        "lease_generation": lease_generation,
    });
    if let Some(resource_limits) = resource_limits {
        response["resource_limits"] = json!(resource_limits);
    }
    (StatusCode::OK, Json(response))
}

async fn defer_runtime_host_resource_limit_claim(
    store: &WorkflowRuntimeStore,
    host_id: &str,
    lease_expires_at: DateTime<Utc>,
    job: &RuntimeJob,
    reason: &str,
) -> (StatusCode, serde_json::Value) {
    let not_before =
        Utc::now() + chrono::TimeDelta::seconds(RESOURCE_LIMIT_CAPABILITY_RETRY_DELAY_SECS);
    match store
        .defer_runtime_job_claim_if_owned(&job.id, host_id, lease_expires_at, not_before)
        .await
    {
        Ok(Some(deferred)) => {
            if let Err(error) = store
                .record_runtime_event(
                    &job.id,
                    "RuntimeJobClaimDeferred",
                    json!({
                        "owner": host_id,
                        "not_before": not_before,
                        "reason": reason,
                        "claim_api": "runtime_host",
                        "required_capability": EVAL_RESOURCE_LIMITS_CAPABILITY,
                    }),
                )
                .await
            {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    host_id = %host_id,
                    %error,
                    "runtime host resource-limit claim defer succeeded but event recording failed"
                );
            }
            (
                StatusCode::OK,
                json!({
                    "claimed": false,
                    "deferred": true,
                    "runtime_job_id": job.id.as_str(),
                    "runtime_job": deferred,
                    "not_before": not_before,
                    "reason": reason,
                    "required_capability": EVAL_RESOURCE_LIMITS_CAPABILITY,
                }),
            )
        }
        Ok(None) => {
            tracing::warn!(
                runtime_job_id = %job.id,
                host_id = %host_id,
                "runtime host resource-limit claim defer ignored because the host no longer owns the lease"
            );
            (StatusCode::OK, json!({ "claimed": false }))
        }
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                host_id = %host_id,
                %error,
                "runtime host failed to defer resource-limit-incompatible runtime job"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("failed to defer runtime job: {error}") }),
            )
        }
    }
}

async fn complete_runtime_host_preflight_failure(
    state: &Arc<AppState>,
    store: &WorkflowRuntimeStore,
    host_id: &str,
    lease_expires_at: DateTime<Utc>,
    job: &RuntimeJob,
    result: ActivityResult,
) -> (StatusCode, Json<serde_json::Value>) {
    let result_payload = match serde_json::to_value(&result) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                %error,
                "failed to serialize remote runtime job preflight result"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to serialize runtime job preflight result" })),
            );
        }
    };
    let completion = match store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &job.id,
            host_id,
            lease_expires_at,
            Some(job.lease_generation),
            &result,
        )
        .await
    {
        Ok(Some(completion)) => completion,
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({
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
                Json(
                    json!({ "error": format!("failed to complete runtime job preflight: {error}") }),
                ),
            );
        }
    };
    if let Err(error) = store
        .record_runtime_event(&job.id, "ActivityResultReady", result_payload)
        .await
    {
        tracing::warn!(
            runtime_job_id = %job.id,
            %error,
            "runtime job preflight completion succeeded but event recording failed"
        );
    }
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
        Json(json!({
            "claimed": false,
            "preflight_failed": true,
            "runtime_job_id": job.id,
            "runtime_job": runtime_job,
            "workflow_event": completion.workflow_event,
            "decision": completion.decision,
        })),
    )
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to load runtime job" })),
            );
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
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid activity result: {e}") })),
            );
        }
    };

    let completion = match store
        .commit_runtime_activity_completion_with_transcript_if_owned_with_generation(
            &runtime_job_id,
            &host_id,
            req.lease_expires_at,
            req.lease_generation,
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
                Json(json!({ "error": format!("failed to complete runtime job: {e}") })),
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

fn runtime_host_supports_eval_resource_limits(state: &Arc<AppState>, host_id: &str) -> bool {
    state.runtime_hosts.hosts.get(host_id).is_some_and(|host| {
        host.capabilities
            .iter()
            .any(|capability| capability == EVAL_RESOURCE_LIMITS_CAPABILITY)
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

fn eval_resource_limit_preflight_failure(
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

fn validate_eval_resource_limit_report(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_resource_limits_derive_from_timeout_when_missing() {
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45
                    }
                }
            }),
        );

        let limits = eval_resource_limit_enforcement_for_job(&job)
            .expect("limits should parse")
            .expect("eval job should require limits");

        assert_eq!(limits.effective.wall_time_secs, Some(45));
        assert_eq!(limits.effective.cpu_time_secs, Some(45));
        assert_eq!(limits.effective.output_bytes, Some(64 * 1024 * 1024));
    }

    #[test]
    fn eval_resource_limit_report_is_required_for_eval_completion() {
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45
                    }
                }
            }),
        );
        let result = ActivityResult::succeeded("implement_issue", "done");

        let err = validate_eval_resource_limit_report(&job, &result)
            .expect_err("missing resource report should fail closed");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1["error"],
            "eval runtime job completion requires resource_limit_report artifact"
        );
    }

    #[test]
    fn eval_resource_limit_report_accepts_matching_usage_evidence() {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45,
                        "resource_limits": limits.clone()
                    }
                }
            }),
        );
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits,
                    usage: harness_sandbox::ResourceUsage {
                        output_bytes: Some(128),
                        wall_time_millis: Some(1000),
                        ..Default::default()
                    },
                    termination: None,
                    reason: "completed within resource limits".to_string(),
                }),
            ),
        );

        validate_eval_resource_limit_report(&job, &result)
            .expect("matching resource report should be accepted");
    }

    #[test]
    fn eval_resource_limit_report_requires_usage_evidence_and_reason() {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45,
                        "resource_limits": limits.clone()
                    }
                }
            }),
        );
        let empty_usage = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits: limits.clone(),
                    usage: harness_sandbox::ResourceUsage::default(),
                    termination: None,
                    reason: "completed within resource limits".to_string(),
                }),
            ),
        );

        let err = validate_eval_resource_limit_report(&job, &empty_usage)
            .expect_err("empty usage should fail closed");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1["error"],
            "resource_limit_report requires usage evidence"
        );

        let empty_reason = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits,
                    usage: harness_sandbox::ResourceUsage {
                        wall_time_millis: Some(1000),
                        ..Default::default()
                    },
                    termination: None,
                    reason: " ".to_string(),
                }),
            ),
        );
        let err = validate_eval_resource_limit_report(&job, &empty_reason)
            .expect_err("empty reason should fail closed");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1["error"],
            "resource_limit_report requires a non-empty reason"
        );
    }

    #[tokio::test]
    async fn register_runtime_host_rejects_required_missing_runtime_state_store(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
        let state_mut =
            Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
        state_mut.startup_statuses =
            vec![
                crate::http::state::StoreStartupResult::optional("runtime_state_store")
                    .failed("pool timed out while waiting for an open connection"),
            ];
        state_mut.degraded_subsystems = vec!["runtime_state_store"];

        let (status, body) = register_runtime_host(
            State(state.clone()),
            Json(RegisterRuntimeHostRequest {
                host_id: "host-a".to_string(),
                display_name: None,
                capabilities: vec![],
            }),
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["error"], "runtime state persistence unavailable");
        assert!(
            !state.runtime_hosts.hosts.contains_key("host-a"),
            "host registration must not mutate memory when required persistence is unavailable"
        );
        assert!(state.is_runtime_state_dirty());
        Ok(())
    }

    #[tokio::test]
    async fn deregister_runtime_host_rejects_required_missing_runtime_state_store_before_lookup(
    ) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
        let state_mut =
            Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
        state_mut.startup_statuses =
            vec![
                crate::http::state::StoreStartupResult::optional("runtime_state_store")
                    .failed("pool timed out while waiting for an open connection"),
            ];
        state_mut.degraded_subsystems = vec!["runtime_state_store"];

        let (status, body) =
            deregister_runtime_host(State(state.clone()), Path("ghost-host".to_string())).await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.0["error"], "runtime state persistence unavailable");
        assert!(state.is_runtime_state_dirty());
        Ok(())
    }
}
