use super::*;

const CAPABILITY_RETRY_DELAY_SECS: i64 = 30;

pub(super) async fn defer_runtime_host_capability_claim(
    store: &WorkflowRuntimeStore,
    host_id: &str,
    lease_expires_at: DateTime<Utc>,
    job: &RuntimeJob,
    reason: &str,
    required_capability: &str,
) -> (StatusCode, serde_json::Value) {
    let not_before = Utc::now() + chrono::TimeDelta::seconds(CAPABILITY_RETRY_DELAY_SECS);
    match store
        .defer_runtime_job_claim_if_owned(&job.id, host_id, lease_expires_at, not_before)
        .await
    {
        Ok(RuntimeJobClaimDeferOutcome::Deferred(deferred)) => {
            if let Err(error) = store
                .record_runtime_event(
                    &job.id,
                    "RuntimeJobClaimDeferred",
                    json!({
                        "owner": host_id,
                        "not_before": not_before,
                        "reason": reason,
                        "claim_api": "runtime_host",
                        "required_capability": required_capability,
                    }),
                )
                .await
            {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    host_id = %host_id,
                    %error,
                    "runtime host capability claim defer succeeded but event recording failed"
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
                    "required_capability": required_capability,
                }),
            )
        }
        Ok(RuntimeJobClaimDeferOutcome::CancellationRequested(cancelled)) => {
            let lease_proof = match store
                .remote_runtime_job_lease_proof(
                    &cancelled.id,
                    host_id,
                    cancelled.lease_generation,
                    lease_expires_at,
                )
                .await
            {
                Ok(Some(proof)) => proof,
                Ok(None) => {
                    let response = lease::workflow_store_unavailable_response();
                    return (response.0, response.1 .0);
                }
                Err(error) => {
                    tracing::error!(
                        runtime_job_id = %cancelled.id,
                        host_id,
                        %error,
                        "cancelled runtime job lease proof lookup failed"
                    );
                    let response = lease::workflow_store_unavailable_response();
                    return (response.0, response.1 .0);
                }
            };
            (
                StatusCode::OK,
                runtime_host_claim(cancelled, lease_expires_at, lease_proof),
            )
        }
        Ok(RuntimeJobClaimDeferOutcome::StaleLease) => {
            tracing::warn!(
                runtime_job_id = %job.id,
                host_id = %host_id,
                "runtime host capability claim defer ignored because the host no longer owns the lease"
            );
            (StatusCode::OK, json!({ "claimed": false }))
        }
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                host_id = %host_id,
                %error,
                "runtime host failed to defer capability-incompatible runtime job"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("failed to defer runtime job: {error}") }),
            )
        }
    }
}
