use crate::http::AppState;
use crate::runtime_hosts::RuntimeHostLifecycle;
use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, TimeDelta, Utc};
use harness_workflow::runtime::store::runtime_job_leases::{
    postgres_timestamp_ceil, RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRequest,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct OptionalLeaseSecs(Option<u64>);

impl OptionalLeaseSecs {
    pub(super) fn value(&self) -> Option<u64> {
        self.0
    }
}

impl<'de> Deserialize<'de> for OptionalLeaseSecs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

#[derive(Debug, Deserialize)]
pub struct ClaimRuntimeJobRequest {
    #[serde(default)]
    pub lease_secs: OptionalLeaseSecs,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenewRuntimeJobLeaseRequest {
    pub lease_generation: u64,
    pub lease_expires_at: DateTime<Utc>,
    pub renewal_id: Uuid,
    #[serde(default)]
    lease_secs: OptionalLeaseSecs,
}

pub async fn renew_runtime_job_lease_for_runtime_host(
    State(state): State<Arc<AppState>>,
    Path((host_id, runtime_job_id)): Path<(String, String)>,
    payload: Result<Json<RenewRuntimeJobLeaseRequest>, JsonRejection>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Json(req) = match payload {
        Ok(payload) => payload,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid lease renewal request" })),
            )
        }
    };
    let _host_operation = state.runtime_hosts.lock_operation(&host_id).await;
    let owner_active = match state.runtime_hosts.lifecycle(&host_id) {
        Some(RuntimeHostLifecycle::Active) => true,
        Some(RuntimeHostLifecycle::Draining) => false,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "runtime host not found" })),
            )
        }
    };
    let lease_secs = match validated_runtime_host_lease_secs(req.lease_secs.value()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(store) = state.core.workflow_runtime_store.clone() else {
        return workflow_store_unavailable_response();
    };
    let outcome = store
        .renew_remote_host_runtime_job_lease(RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &runtime_job_id,
            owner: &host_id,
            lease_generation: req.lease_generation,
            previous_expires_at: req.lease_expires_at,
            renewal_id: req.renewal_id,
            lease_secs,
            now: Utc::now(),
            max_lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            owner_active,
        })
        .await;
    match outcome {
        Ok(RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_generation,
            lease_expires_at,
            replayed,
        }) => (
            StatusCode::OK,
            Json(json!({
                "renewed": true,
                "runtime_job_id": runtime_job_id,
                "lease_generation": lease_generation,
                "lease_expires_at": lease_expires_at,
                "replayed": replayed,
            })),
        ),
        Ok(RuntimeJobLeaseRenewalOutcome::LeaseLost { .. }) => lease_lost_response(),
        Ok(RuntimeJobLeaseRenewalOutcome::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "runtime job not found" })),
        ),
        Err(error) => {
            tracing::error!(
                host_id = %host_id,
                runtime_job_id = %runtime_job_id,
                error = %error,
                "runtime host lease renewal failed"
            );
            workflow_store_unavailable_response()
        }
    }
}

pub(super) fn runtime_host_lease_expires_at(
    lease_secs: Option<u64>,
) -> Result<DateTime<Utc>, (StatusCode, Json<serde_json::Value>)> {
    runtime_host_lease_expires_at_at(Utc::now(), lease_secs)
}

fn runtime_host_lease_expires_at_at(
    now: DateTime<Utc>,
    lease_secs: Option<u64>,
) -> Result<DateTime<Utc>, (StatusCode, Json<serde_json::Value>)> {
    let lease_secs = validated_runtime_host_lease_secs(lease_secs)?;
    let duration = TimeDelta::try_seconds(lease_secs).ok_or_else(invalid_duration_response)?;
    now.checked_add_signed(duration)
        .and_then(postgres_timestamp_ceil)
        .ok_or_else(invalid_duration_response)
}

fn validated_runtime_host_lease_secs(
    lease_secs: Option<u64>,
) -> Result<i64, (StatusCode, Json<serde_json::Value>)> {
    let value = lease_secs.unwrap_or(crate::runtime_hosts::DEFAULT_LEASE_SECS as u64);
    if !(1..=crate::runtime_hosts::MAX_LEASE_SECS as u64).contains(&value) {
        return Err(invalid_duration_response());
    }
    Ok(value as i64)
}

fn invalid_duration_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "lease_secs must be between 1 and 3600" })),
    )
}

pub(super) fn lease_lost_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error_code": "lease_lost",
            "must_stop": true,
        })),
    )
}

fn workflow_store_unavailable_response() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "workflow runtime store unavailable" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_job_lease_duration_accepts_default_and_boundaries() {
        let now = DateTime::parse_from_rfc3339("2026-07-16T00:00:00Z")
            .expect("fixed timestamp must parse")
            .to_utc();
        assert_eq!(
            runtime_host_lease_expires_at_at(now, None).expect("default duration must be valid"),
            now + TimeDelta::seconds(60)
        );
        assert_eq!(
            runtime_host_lease_expires_at_at(now, Some(1)).expect("minimum duration must be valid"),
            now + TimeDelta::seconds(1)
        );
        assert_eq!(
            runtime_host_lease_expires_at_at(now, Some(3600))
                .expect("maximum duration must be valid"),
            now + TimeDelta::seconds(3600)
        );
    }

    #[test]
    fn runtime_job_lease_duration_ceils_submicrosecond_server_time() {
        use chrono::Timelike;

        let now = DateTime::parse_from_rfc3339("2026-07-16T00:00:00.123456789Z")
            .expect("fixed timestamp must parse")
            .to_utc();
        let raw_expiry = now + TimeDelta::seconds(60);
        let expiry = runtime_host_lease_expires_at_at(now, Some(60))
            .expect("normalized duration must be valid");

        assert!(expiry >= raw_expiry);
        assert!(expiry - raw_expiry < TimeDelta::microseconds(1));
        assert_eq!(expiry.nanosecond() % 1_000, 0);
    }

    #[test]
    fn runtime_job_lease_duration_rejects_zero_oversized_and_null() {
        assert!(validated_runtime_host_lease_secs(Some(0)).is_err());
        assert!(validated_runtime_host_lease_secs(Some(3601)).is_err());
        assert!(validated_runtime_host_lease_secs(Some(u64::MAX)).is_err());

        let request = json!({
            "lease_generation": 1,
            "lease_expires_at": "2026-07-16T00:00:00Z",
            "renewal_id": Uuid::nil(),
            "lease_secs": null,
        });
        assert!(serde_json::from_value::<RenewRuntimeJobLeaseRequest>(request).is_err());
    }

    #[test]
    fn runtime_job_lease_lost_response_is_stable_and_sanitized() {
        let (status, body) = lease_lost_response();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body.0,
            json!({ "error_code": "lease_lost", "must_stop": true })
        );
    }
}
