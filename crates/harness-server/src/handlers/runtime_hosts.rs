use crate::http::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, TimeDelta, Utc};
use harness_workflow::runtime::{
    ActivityResult, RuntimeJobNotFoundError, RuntimeKind, WorkflowRuntimeStore,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RegisterRuntimeHostRequest {
    pub host_id: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimTaskRequest {
    pub lease_secs: Option<u64>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteRuntimeJobRequest {
    pub lease_expires_at: DateTime<Utc>,
    pub result: ActivityResult,
}

pub async fn list_runtime_hosts(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let hosts = state.runtime_hosts.list_hosts();
    (StatusCode::OK, Json(json!({ "hosts": hosts })))
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
        match state.core.tasks.release_runtime_host_claims(&host_id).await {
            Ok(released) => {
                if !released.is_empty() {
                    tracing::info!(
                        host_id = %host_id,
                        released_tasks = released.len(),
                        "runtime host deregistration released pending scheduler leases"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    host_id = %host_id,
                    error = %e,
                    "failed to release runtime-host-owned task leases during deregistration"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": format!("failed to release task leases for runtime host: {e}")
                    })),
                );
            }
        }

        if !state.runtime_hosts.deregister(&host_id) {
            tracing::warn!(
                host_id = %host_id,
                "runtime host disappeared during deregistration after task lease release"
            );
        }
        state.runtime_project_cache.clear_host(&host_id);
        if let Err(response) = persist_runtime_state(&state).await {
            return response;
        }
        (StatusCode::OK, Json(json!({ "deregistered": true })))
    }
}

pub async fn claim_task_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
    Json(req): Json<ClaimTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
    }

    let project_filter = req
        .project
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut tasks: Vec<(crate::task_runner::TaskId, Option<String>, Option<String>)> = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .filter(|task| task.status.as_ref() == "pending")
        .map(|task| {
            (
                task.id,
                task.created_at,
                task.project_root.map(|p| p.to_string_lossy().into_owned()),
            )
        })
        .filter(|(_, _, project)| match project_filter {
            Some(filter) => project.as_deref() == Some(filter),
            None => true,
        })
        .collect();
    tasks.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));

    let lease_secs = match req.lease_secs {
        Some(value) => match i64::try_from(value) {
            Ok(v) => Some(v),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "lease_secs must be <= i64::MAX" })),
                )
            }
        },
        None => None,
    };

    for (task_id, _, _) in tasks {
        match state
            .core
            .tasks
            .claim_for_runtime_host(&task_id, &host_id, lease_secs)
            .await
        {
            Ok(Some(claim)) => {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "claimed": true,
                        "task_id": claim.task_id,
                        "lease_expires_at": claim.lease_expires_at
                    })),
                );
            }
            Ok(None) => continue,
            Err(e) => {
                let message = e.to_string();
                if message.contains("too large to compute a valid expiration timestamp") {
                    return (StatusCode::BAD_REQUEST, Json(json!({ "error": message })));
                }
                tracing::error!(
                    host_id = %host_id,
                    task_id = %task_id,
                    error = %message,
                    "runtime host claim failed to persist authoritative scheduler state"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("failed to claim task: {message}") })),
                );
            }
        }
    }

    (StatusCode::OK, Json(json!({ "claimed": false })))
}

pub async fn claim_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path(host_id): Path<String>,
    Json(req): Json<ClaimTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
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
    let lease_expires_at = match runtime_host_lease_expires_at(req.lease_secs) {
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

    if let Err(e) = store
        .record_runtime_event(
            &job.id,
            "RuntimeJobClaimed",
            json!({
                "owner": host_id.as_str(),
                "lease_expires_at": lease_expires_at,
                "claim_api": "runtime_host",
            }),
        )
        .await
    {
        tracing::warn!(
            runtime_job_id = %job.id,
            error = %e,
            "runtime host claim succeeded but runtime event recording failed"
        );
    }

    let runtime_job_id = job.id.clone();
    (
        StatusCode::OK,
        Json(json!({
            "claimed": true,
            "runtime_job": job,
            "runtime_job_id": runtime_job_id,
            "lease_expires_at": lease_expires_at,
        })),
    )
}

pub async fn complete_runtime_job_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path((host_id, runtime_job_id)): Path<(String, String)>,
    Json(req): Json<CompleteRuntimeJobRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !state.runtime_hosts.hosts.contains_key(&host_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("runtime host '{host_id}' is not registered") })),
        );
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
        .commit_runtime_activity_completion_if_owned(
            &runtime_job_id,
            &host_id,
            req.lease_expires_at,
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

    (
        StatusCode::OK,
        Json(json!({
            "completed": true,
            "runtime_job": completion.runtime_job,
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

fn runtime_host_lease_expires_at(
    lease_secs: Option<u64>,
) -> Result<DateTime<Utc>, (StatusCode, Json<serde_json::Value>)> {
    let lease_secs = match lease_secs {
        Some(value) => match i64::try_from(value) {
            Ok(value) => value,
            Err(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "lease_secs must be <= i64::MAX" })),
                ));
            }
        },
        None => crate::runtime_hosts::DEFAULT_LEASE_SECS,
    }
    .max(0);
    let lease_duration = match TimeDelta::try_seconds(lease_secs) {
        Some(value) => value,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!(
                        "lease_secs value {lease_secs} is too large to compute a valid expiration timestamp"
                    )
                })),
            ));
        }
    };
    Utc::now().checked_add_signed(lease_duration).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "lease_secs value {lease_secs} is too large to compute a valid expiration timestamp"
                )
            })),
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
