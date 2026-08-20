use super::*;
use harness_workflow::runtime::store::runtime_job_leases::{
    RuntimeJobLeaseRenewalOutcome, RuntimeJobLeaseRenewalRequest,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(super) enum CompletionLeaseReservation {
    Reserved {
        lease_expires_at: DateTime<Utc>,
        lease_generation: u64,
        lease_proof: Uuid,
    },
    IssuedButStale {
        lease_expires_at: DateTime<Utc>,
        lease_generation: u64,
        lease_proof: Option<Uuid>,
    },
}

impl CompletionLeaseReservation {
    pub(super) fn completion_lease<'a>(&self, owner: &'a str) -> RuntimeJobCompletionLease<'a> {
        match self {
            Self::Reserved {
                lease_expires_at,
                lease_generation,
                lease_proof,
            } => RuntimeJobCompletionLease::remote(
                owner,
                *lease_expires_at,
                *lease_generation,
                Some(*lease_proof),
            ),
            Self::IssuedButStale {
                lease_expires_at,
                lease_generation,
                lease_proof,
            } => RuntimeJobCompletionLease::remote(
                owner,
                *lease_expires_at,
                *lease_generation,
                *lease_proof,
            ),
        }
    }

    pub(super) fn is_issued_but_stale(&self) -> bool {
        matches!(self, Self::IssuedButStale { .. })
    }

    pub(super) fn error_response(&self, error: impl Into<String>) -> CompletionJson {
        let error = error.into();
        match self {
            Self::Reserved {
                lease_expires_at,
                lease_generation,
                lease_proof,
            } => reserved_lease_error_response(
                error,
                *lease_expires_at,
                *lease_generation,
                *lease_proof,
            ),
            Self::IssuedButStale { .. } => completion_json(json!({ "error": error })),
        }
    }
}

pub(super) fn reserved_lease_error_response(
    error: impl Into<String>,
    lease_expires_at: DateTime<Utc>,
    lease_generation: u64,
    lease_proof: Uuid,
) -> CompletionJson {
    completion_json(json!({
        "error": error.into(),
        "lease_expires_at": lease_expires_at,
        "lease_generation": lease_generation,
        "lease_proof": lease_proof,
        "lease_reserved": true,
    }))
}

pub(super) async fn reserve_completion_lease(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
    owner: &str,
    previous_expires_at: DateTime<Utc>,
    lease_generation: Option<u64>,
    lease_proof: Option<Uuid>,
    cancellation_ack: bool,
    owner_active: bool,
) -> Result<CompletionLeaseReservation, (StatusCode, CompletionJson)> {
    let lease_generation = lease_generation.unwrap_or(job.lease_generation);
    let renewal_id =
        completion_reservation_id(&job.id, owner, lease_generation, previous_expires_at);
    let now = Utc::now();
    let renew = || async {
        let request = RuntimeJobLeaseRenewalRequest {
            runtime_job_id: &job.id,
            owner,
            lease_generation,
            lease_proof,
            previous_expires_at,
            renewal_id,
            lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            now,
            max_lease_secs: crate::runtime_hosts::MAX_LEASE_SECS,
            owner_active,
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
        }) => match store
            .remote_runtime_job_lease_proof(&job.id, owner, lease_generation, lease_expires_at)
            .await
        {
            Ok(Some(lease_proof)) => Ok(CompletionLeaseReservation::Reserved {
                lease_expires_at,
                lease_generation,
                lease_proof,
            }),
            Ok(None) => {
                let (status, body) = lease::workflow_store_unavailable_response();
                Err((status, completion_json(body.0)))
            }
            Err(error) => {
                tracing::error!(
                    runtime_job_id = %job.id,
                    owner,
                    %error,
                    "reserved runtime job lease proof lookup failed"
                );
                let (status, body) = lease::workflow_store_unavailable_response();
                Err((status, completion_json(body.0)))
            }
        },
        Ok(RuntimeJobLeaseRenewalOutcome::LeaseLost { .. }) => match store
            .remote_stale_completion_is_issued(
                &job.id,
                RuntimeJobCompletionLease::remote(
                    owner,
                    previous_expires_at,
                    lease_generation,
                    lease_proof,
                ),
            )
            .await
        {
            Ok(true) => Ok(CompletionLeaseReservation::IssuedButStale {
                lease_expires_at: previous_expires_at,
                lease_generation,
                lease_proof,
            }),
            Ok(false) => {
                let (status, body) = lease::lease_lost_response();
                Err((status, completion_json(body.0)))
            }
            Err(error) => {
                tracing::error!(
                    runtime_job_id = %job.id,
                    owner,
                    %error,
                    "runtime host stale completion issuance lookup failed"
                );
                let (status, body) = lease::workflow_store_unavailable_response();
                Err((status, completion_json(body.0)))
            }
        },
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

pub(crate) async fn replay_completion_reservation(
    store: &WorkflowRuntimeStore,
    runtime_job_id: &str,
    owner: &str,
    lease_generation: u64,
    previous_expires_at: DateTime<Utc>,
    lease_proof: Option<Uuid>,
) -> anyhow::Result<RuntimeJobLeaseRenewalOutcome> {
    store
        .renew_remote_host_runtime_job_lease(RuntimeJobLeaseRenewalRequest {
            runtime_job_id,
            owner,
            lease_generation,
            lease_proof,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_reservation_errors_return_the_updated_lease_fence() {
        let lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
        let proof = Uuid::new_v4();
        let response =
            reserved_lease_error_response("verification failed", lease_expires_at, 7, proof);
        assert_eq!(response.0["error"], "verification failed");
        assert_eq!(response.0["lease_expires_at"], json!(lease_expires_at));
        assert_eq!(response.0["lease_generation"], 7);
        assert_eq!(response.0["lease_proof"], json!(proof));
        assert_eq!(response.0["lease_reserved"], true);
    }

    #[test]
    fn completion_reservation_id_is_stable_for_ambiguous_retries() {
        let lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
        let first = completion_reservation_id("job-1", "host-a", 3, lease_expires_at);
        let retry = completion_reservation_id("job-1", "host-a", 3, lease_expires_at);
        let different_owner = completion_reservation_id("job-1", "host-b", 3, lease_expires_at);
        assert_eq!(first, retry);
        assert_ne!(first, different_owner);
    }
}
