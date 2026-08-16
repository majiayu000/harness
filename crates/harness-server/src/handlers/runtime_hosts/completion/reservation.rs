use super::*;
use harness_workflow::runtime::store::runtime_job_leases::{
    RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRequest,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(super) fn reserved_lease_error_response(
    error: impl Into<String>,
    lease_expires_at: DateTime<Utc>,
    lease_generation: u64,
) -> CompletionJson {
    completion_json(json!({
        "error": error.into(),
        "lease_expires_at": lease_expires_at,
        "lease_generation": lease_generation,
        "lease_reserved": true,
    }))
}

pub(super) async fn reserve_completion_lease(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
    owner: &str,
    previous_expires_at: DateTime<Utc>,
    lease_generation: Option<u64>,
    cancellation_ack: bool,
) -> Result<(DateTime<Utc>, u64), (StatusCode, CompletionJson)> {
    let lease_generation = lease_generation.unwrap_or(job.lease_generation);
    let renewal_id =
        completion_reservation_id(&job.id, owner, lease_generation, previous_expires_at);
    let now = Utc::now();
    let renew = || async {
        let request = RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &job.id,
            owner,
            lease_generation,
            previous_expires_at,
            renewal_id,
            lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            now,
            max_lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            owner_active: true,
        };
        if cancellation_ack {
            store
                .reserve_cancelled_remote_host_runtime_job_completion(request)
                .await
        } else {
            store.renew_remote_host_runtime_job_lease(request).await
        }
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
        Ok(RuntimeJobLeaseRenewalOutcome::LeaseLost { .. }) => {
            let (status, body) = lease::lease_lost_response();
            Err((status, completion_json(body.0)))
        }
        Ok(RuntimeJobLeaseRenewalOutcome::NotFound) => Err((
            StatusCode::NOT_FOUND,
            completion_json(json!({ "error": "runtime job not found" })),
        )),
        Err(error) => {
            tracing::error!(
                runtime_job_id = %job.id,
                owner,
                %error,
                "runtime host failed to reserve completion lease"
            );
            let (status, body) = lease::workflow_store_unavailable_response();
            Err((status, completion_json(body.0)))
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

pub(in crate::handlers::runtime_hosts) async fn replay_completion_reservation(
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
