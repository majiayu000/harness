use super::{enum_str, to_jsonb_string, WorkflowRuntimeStore};
use crate::runtime::model::{RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind};
use chrono::{DateTime, TimeDelta, Timelike, Utc};
use serde_json::{json, Value};
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeJobLeaseRenewalRejection {
    Expired,
    HostDraining,
    InvalidDuration,
    InvariantViolation,
    RenewalIdConflict,
    Revoked,
    StaleExpiry,
    WrongGeneration,
    WrongOwner,
    WrongRuntimeKind,
    WrongState,
}

impl RuntimeJobLeaseRenewalRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::HostDraining => "host_draining",
            Self::InvalidDuration => "invalid_duration",
            Self::InvariantViolation => "invariant_violation",
            Self::RenewalIdConflict => "renewal_id_conflict",
            Self::Revoked => "revoked",
            Self::StaleExpiry => "stale_expiry",
            Self::WrongGeneration => "wrong_generation",
            Self::WrongOwner => "wrong_owner",
            Self::WrongRuntimeKind => "wrong_runtime_kind",
            Self::WrongState => "wrong_state",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeJobLeaseRenewalOutcome {
    Renewed {
        lease_generation: u64,
        lease_expires_at: DateTime<Utc>,
        replayed: bool,
    },
    LeaseLost {
        reason: RuntimeJobLeaseRenewalRejection,
    },
    NotFound,
}

#[derive(Debug, Clone)]
pub struct RuntimeJobLeaseRenewalRequest<'a> {
    pub runtime_job_id: &'a str,
    pub owner: &'a str,
    pub lease_generation: u64,
    pub previous_expires_at: DateTime<Utc>,
    pub renewal_id: Uuid,
    pub lease_secs: i64,
    pub now: DateTime<Utc>,
    pub max_lease_secs: i64,
    pub owner_active: bool,
}

type ReceiptRow = (String, DateTime<Utc>, DateTime<Utc>, i64);

pub fn postgres_timestamp_floor(value: DateTime<Utc>) -> DateTime<Utc> {
    let microsecond_nanos = value.nanosecond() / 1_000 * 1_000;
    value.with_nanosecond(microsecond_nanos).unwrap_or(value)
}

pub fn postgres_timestamp_ceil(value: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let remainder = value.nanosecond() % 1_000;
    if remainder == 0 {
        return Some(value);
    }
    value
        .checked_add_signed(TimeDelta::nanoseconds(i64::from(1_000 - remainder)))
        .map(postgres_timestamp_floor)
}

impl WorkflowRuntimeStore {
    pub async fn renew_remote_host_runtime_job_lease(
        &self,
        request: RuntimeJobLeaseRenewalRequest<'_>,
    ) -> anyhow::Result<RuntimeJobLeaseRenewalOutcome> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(request.runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            tx.commit().await?;
            return Ok(RuntimeJobLeaseRenewalOutcome::NotFound);
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let request_previous_expires_at = postgres_timestamp_floor(request.previous_expires_at);

        sqlx::query(
            "DELETE FROM runtime_job_lease_renewal_receipts
             WHERE runtime_job_id = $1 AND renewed_expires_at <= $2",
        )
        .bind(request.runtime_job_id)
        .bind(request.now)
        .execute(&mut *tx)
        .await?;

        let rejection = if !request.owner_active {
            Some(RuntimeJobLeaseRenewalRejection::HostDraining)
        } else if job.status != RuntimeJobStatus::Running {
            Some(RuntimeJobLeaseRenewalRejection::WrongState)
        } else if job.runtime_kind != RuntimeKind::RemoteHost {
            Some(RuntimeJobLeaseRenewalRejection::WrongRuntimeKind)
        } else if job.lease_generation != request.lease_generation {
            Some(RuntimeJobLeaseRenewalRejection::WrongGeneration)
        } else {
            match job.lease.as_ref() {
                None => Some(RuntimeJobLeaseRenewalRejection::Revoked),
                Some(lease) if lease.expires_at <= request.now => {
                    Some(RuntimeJobLeaseRenewalRejection::Expired)
                }
                Some(lease) if lease.owner != request.owner => {
                    Some(RuntimeJobLeaseRenewalRejection::WrongOwner)
                }
                Some(_) => None,
            }
        };
        if let Some(reason) = rejection {
            return reject_renewal_tx(tx, &request, reason).await;
        }

        let generation = match i64::try_from(request.lease_generation) {
            Ok(generation) => generation,
            Err(_) => {
                return reject_renewal_tx(
                    tx,
                    &request,
                    RuntimeJobLeaseRenewalRejection::InvariantViolation,
                )
                .await;
            }
        };
        let receipt: Option<ReceiptRow> = sqlx::query_as(
            "SELECT owner, previous_expires_at, renewed_expires_at, lease_secs
             FROM runtime_job_lease_renewal_receipts
             WHERE runtime_job_id = $1 AND lease_generation = $2 AND renewal_id = $3",
        )
        .bind(request.runtime_job_id)
        .bind(generation)
        .bind(request.renewal_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((owner, previous_expires_at, renewed_expires_at, lease_secs)) = receipt {
            if owner != request.owner
                || previous_expires_at != request_previous_expires_at
                || lease_secs != request.lease_secs
            {
                return reject_renewal_tx(
                    tx,
                    &request,
                    RuntimeJobLeaseRenewalRejection::RenewalIdConflict,
                )
                .await;
            }
            tx.commit().await?;
            return Ok(RuntimeJobLeaseRenewalOutcome::Renewed {
                lease_generation: job.lease_generation,
                lease_expires_at: renewed_expires_at,
                replayed: true,
            });
        }

        let Some(current_expires_at) = job.lease.as_ref().map(|lease| lease.expires_at) else {
            return reject_renewal_tx(tx, &request, RuntimeJobLeaseRenewalRejection::Revoked).await;
        };
        if postgres_timestamp_floor(current_expires_at) != request_previous_expires_at {
            return reject_renewal_tx(tx, &request, RuntimeJobLeaseRenewalRejection::StaleExpiry)
                .await;
        }
        if request.lease_secs <= 0 || request.lease_secs > request.max_lease_secs {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvalidDuration,
            )
            .await;
        }
        let Some(max_duration) = TimeDelta::try_seconds(request.max_lease_secs) else {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvariantViolation,
            )
            .await;
        };
        let Some(max_expires_at) = request
            .now
            .checked_add_signed(max_duration)
            .and_then(postgres_timestamp_ceil)
        else {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvariantViolation,
            )
            .await;
        };
        let Some(normalized_current_expires_at) = postgres_timestamp_ceil(current_expires_at)
        else {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvariantViolation,
            )
            .await;
        };
        if normalized_current_expires_at > max_expires_at {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvariantViolation,
            )
            .await;
        }
        let Some(target_duration) = TimeDelta::try_seconds(request.lease_secs) else {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvalidDuration,
            )
            .await;
        };
        let Some(target_expires_at) = request
            .now
            .checked_add_signed(target_duration)
            .and_then(postgres_timestamp_ceil)
        else {
            return reject_renewal_tx(
                tx,
                &request,
                RuntimeJobLeaseRenewalRejection::InvalidDuration,
            )
            .await;
        };
        let renewed_expires_at = normalized_current_expires_at.max(target_expires_at);
        job.renew_lease(request.owner, renewed_expires_at);
        job.updated_at = request.now;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = $4
             WHERE id = $5",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(request.now)
        .bind(request.runtime_job_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO runtime_job_lease_renewal_receipts
                (runtime_job_id, renewal_id, owner, lease_generation,
                 previous_expires_at, renewed_expires_at, lease_secs, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(request.runtime_job_id)
        .bind(request.renewal_id)
        .bind(request.owner)
        .bind(generation)
        .bind(request_previous_expires_at)
        .bind(renewed_expires_at)
        .bind(request.lease_secs)
        .bind(request.now)
        .execute(&mut *tx)
        .await?;
        append_runtime_event_tx(
            &mut tx,
            request.runtime_job_id,
            "RuntimeJobLeaseRenewed",
            json!({
                "owner": request.owner,
                "lease_generation": request.lease_generation,
                "previous_expires_at": request_previous_expires_at,
                "lease_expires_at": renewed_expires_at,
                "lease_secs": request.lease_secs,
                "renewal_id": request.renewal_id,
                "source": "runtime_host",
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_generation: job.lease_generation,
            lease_expires_at: renewed_expires_at,
            replayed: false,
        })
    }

    pub async fn revoke_remote_host_runtime_job_leases(
        &self,
        owner: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE status = 'running'
               AND runtime_kind = 'remote_host'
               AND data #>> '{lease,owner}' = $1
             ORDER BY id
             FOR UPDATE",
        )
        .bind(owner)
        .fetch_all(&mut *tx)
        .await?;
        for (runtime_job_id, data) in &rows {
            let mut job: RuntimeJob = serde_json::from_str(data)?;
            let previous_expires_at = job.lease.as_ref().map(|lease| lease.expires_at);
            job.status = RuntimeJobStatus::Pending;
            job.lease = None;
            job.not_before = None;
            job.updated_at = now;
            let updated = to_jsonb_string(&job)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET status = 'pending', not_before = NULL, data = $1::jsonb, updated_at = $2
                 WHERE id = $3",
            )
            .bind(&updated)
            .bind(now)
            .bind(runtime_job_id)
            .execute(&mut *tx)
            .await?;
            delete_runtime_job_lease_receipts_tx(&mut tx, runtime_job_id, job.lease_generation)
                .await?;
            append_runtime_event_tx(
                &mut tx,
                runtime_job_id,
                "RuntimeJobLeaseRevoked",
                json!({
                    "owner": owner,
                    "lease_generation": job.lease_generation,
                    "previous_expires_at": previous_expires_at,
                    "reason": "host_deregistered",
                    "source": "runtime_host_deregister",
                }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len())
    }

    pub async fn count_remote_host_runtime_job_leases(&self, owner: &str) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM runtime_jobs
             WHERE status = 'running'
               AND runtime_kind = 'remote_host'
               AND data #>> '{lease,owner}' = $1",
        )
        .bind(owner)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn count_remote_host_runtime_job_leases_by_owner(
        &self,
    ) -> anyhow::Result<BTreeMap<String, i64>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT data #>> '{lease,owner}' AS owner, COUNT(*) AS count
             FROM runtime_jobs
             WHERE status = 'running'
               AND runtime_kind = 'remote_host'
               AND data #>> '{lease,owner}' IS NOT NULL
             GROUP BY data #>> '{lease,owner}'",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }
}

async fn reject_renewal_tx(
    mut tx: Transaction<'_, Postgres>,
    request: &RuntimeJobLeaseRenewalRequest<'_>,
    reason: RuntimeJobLeaseRenewalRejection,
) -> anyhow::Result<RuntimeJobLeaseRenewalOutcome> {
    append_runtime_event_tx(
        &mut tx,
        request.runtime_job_id,
        "RuntimeJobLeaseRenewalRejected",
        json!({
            "requested_lease_generation": request.lease_generation,
            "renewal_id": request.renewal_id,
            "reason": reason.as_str(),
            "source": "runtime_host",
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(RuntimeJobLeaseRenewalOutcome::LeaseLost { reason })
}

pub(crate) async fn append_runtime_event_tx(
    tx: &mut Transaction<'_, Postgres>,
    runtime_job_id: &str,
    event_type: &str,
    payload: Value,
) -> anyhow::Result<RuntimeEvent> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("runtime_events:{runtime_job_id}"))
        .execute(&mut **tx)
        .await?;
    let (next_sequence,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM runtime_events WHERE runtime_job_id = $1",
    )
    .bind(runtime_job_id)
    .fetch_one(&mut **tx)
    .await?;
    let event = RuntimeEvent::new(runtime_job_id, next_sequence as u64, event_type, payload);
    let data = to_jsonb_string(&event)?;
    sqlx::query(
        "INSERT INTO runtime_events (id, runtime_job_id, sequence, event_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind(&event.id)
    .bind(&event.runtime_job_id)
    .bind(event.sequence as i64)
    .bind(&event.event_type)
    .bind(&data)
    .execute(&mut **tx)
    .await?;
    Ok(event)
}

pub(crate) async fn delete_runtime_job_lease_receipts_tx(
    tx: &mut Transaction<'_, Postgres>,
    runtime_job_id: &str,
    lease_generation: u64,
) -> anyhow::Result<()> {
    let generation = i64::try_from(lease_generation)
        .map_err(|_| anyhow::anyhow!("lease generation exceeds PostgreSQL BIGINT range"))?;
    sqlx::query(
        "DELETE FROM runtime_job_lease_renewal_receipts
         WHERE runtime_job_id = $1 AND lease_generation = $2",
    )
    .bind(runtime_job_id)
    .bind(generation)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn delete_all_runtime_job_lease_receipts_tx(
    tx: &mut Transaction<'_, Postgres>,
    runtime_job_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM runtime_job_lease_renewal_receipts WHERE runtime_job_id = $1")
        .bind(runtime_job_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RuntimeJobLeaseRenewalRejection;

    #[test]
    fn runtime_store_remote_host_lease_rejection_codes_are_closed_and_sanitized() {
        let reasons = [
            RuntimeJobLeaseRenewalRejection::Expired,
            RuntimeJobLeaseRenewalRejection::HostDraining,
            RuntimeJobLeaseRenewalRejection::InvalidDuration,
            RuntimeJobLeaseRenewalRejection::InvariantViolation,
            RuntimeJobLeaseRenewalRejection::RenewalIdConflict,
            RuntimeJobLeaseRenewalRejection::Revoked,
            RuntimeJobLeaseRenewalRejection::StaleExpiry,
            RuntimeJobLeaseRenewalRejection::WrongGeneration,
            RuntimeJobLeaseRenewalRejection::WrongOwner,
            RuntimeJobLeaseRenewalRejection::WrongRuntimeKind,
            RuntimeJobLeaseRenewalRejection::WrongState,
        ];
        let codes: Vec<&str> = reasons.into_iter().map(|reason| reason.as_str()).collect();
        assert_eq!(codes.len(), 11);
        assert!(codes.iter().all(|code| !code.contains("host-")));
    }
}
