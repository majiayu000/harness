use crate::http::AppState;
use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use harness_workflow::runtime::{
    ActivityResult, RuntimeJobClaimDecision, RuntimeJobClaimGuard, RuntimeJobNotFoundError,
    RuntimeKind, WorkflowRuntimeStore,
};
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

mod lease;
pub use lease::renew_runtime_job_lease_for_runtime_host;

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
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "workflow runtime store unavailable" })),
            );
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
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "workflow runtime store unavailable" })),
                );
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

    let job = match store
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

    let runtime_job_id = job.id.clone();
    let lease_generation = job.lease_generation;
    (
        StatusCode::OK,
        Json(json!({
            "claimed": true,
            "runtime_job": job,
            "runtime_job_id": runtime_job_id,
            "lease_expires_at": lease_expires_at,
            "lease_generation": lease_generation,
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
    let result_payload = match serde_json::to_value(&req.result) {
        Ok(value) => value,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid activity result: {e}") })),
            );
        }
    };

    let completion = match store
        .commit_runtime_activity_completion_if_owned_with_generation(
            &runtime_job_id,
            &host_id,
            req.lease_expires_at,
            req.lease_generation,
            &req.result,
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
    state.core.workflow_runtime_store.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        )
    })
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
