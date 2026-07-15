use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchBackoffPolicy {
    floor: chrono::Duration,
    ceiling: chrono::Duration,
}

impl Default for DispatchBackoffPolicy {
    fn default() -> Self {
        Self {
            floor: chrono::Duration::seconds(30),
            ceiling: chrono::Duration::minutes(15),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchClaim<'a> {
    pub owner: &'a str,
    pub generation: u64,
}

impl DispatchBackoffPolicy {
    pub fn new(floor: chrono::Duration, ceiling: chrono::Duration) -> anyhow::Result<Self> {
        if floor <= chrono::Duration::zero() || ceiling < floor {
            anyhow::bail!("dispatch defer backoff requires a positive floor and ceiling >= floor");
        }
        Ok(Self { floor, ceiling })
    }

    pub fn from_seconds(floor: u64, ceiling: u64) -> anyhow::Result<Self> {
        let floor = i64::try_from(floor)
            .map_err(|_| anyhow::anyhow!("dispatch defer backoff floor exceeds i64"))?;
        let ceiling = i64::try_from(ceiling)
            .map_err(|_| anyhow::anyhow!("dispatch defer backoff ceiling exceeds i64"))?;
        Self::new(
            chrono::Duration::seconds(floor),
            chrono::Duration::seconds(ceiling),
        )
    }

    pub fn delay_for_attempt(self, attempt: u64) -> chrono::Duration {
        let exponent = attempt.saturating_sub(1).min(62) as u32;
        let multiplier = 1_i64.checked_shl(exponent).unwrap_or(i64::MAX);
        let floor_ms = self.floor.num_milliseconds();
        let ceiling_ms = self.ceiling.num_milliseconds();
        chrono::Duration::milliseconds(floor_ms.saturating_mul(multiplier).min(ceiling_ms))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchBarrierReasonCode {
    RuntimePolicyDisabled,
    WorkflowConfigInvalid,
    IsolationTierUnavailable,
}

impl DispatchBarrierReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RuntimePolicyDisabled => "runtime_policy_disabled",
            Self::WorkflowConfigInvalid => "workflow_config_invalid",
            Self::IsolationTierUnavailable => "isolation_tier_unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchBarrier {
    pub reason_code: DispatchBarrierReasonCode,
    pub reason: String,
    pub project_id: String,
    pub command_id: String,
    pub workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_class: Option<String>,
    pub dispatch_owner: String,
    pub claim_generation: u64,
    pub attempt: u64,
    pub next_dispatch_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBarrierInput {
    pub reason_code: DispatchBarrierReasonCode,
    pub reason: String,
    pub project_id: String,
    pub required_tier: Option<String>,
    pub trust_class: Option<String>,
}

impl DispatchBarrierInput {
    pub fn new(
        reason_code: DispatchBarrierReasonCode,
        reason: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self {
            reason_code,
            reason: reason.into(),
            project_id: project_id.into(),
            required_tier: None,
            trust_class: None,
        }
    }

    pub fn with_isolation(
        mut self,
        required_tier: impl Into<String>,
        trust_class: impl Into<String>,
    ) -> Self {
        self.required_tier = Some(required_tier.into());
        self.trust_class = Some(trust_class.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferClaimedCommandOutcome {
    Deferred(DispatchBarrier),
    AlreadyDeferred(DispatchBarrier),
    StaleClaim,
    WorkflowTerminal {
        status: super::WorkflowCommandStatus,
    },
}

type ClaimedCommandRow = (String, Option<String>, i64, i64, Option<String>);

impl DispatchBarrier {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        reason_code: DispatchBarrierReasonCode,
        reason: impl Into<String>,
        project_id: impl Into<String>,
        command_id: impl Into<String>,
        workflow_id: impl Into<String>,
        required_tier: Option<String>,
        trust_class: Option<String>,
        dispatch_owner: impl Into<String>,
        claim_generation: u64,
        attempt: u64,
        next_dispatch_at: DateTime<Utc>,
    ) -> anyhow::Result<Self> {
        let barrier = Self {
            reason_code,
            reason: reason.into(),
            project_id: project_id.into(),
            command_id: command_id.into(),
            workflow_id: workflow_id.into(),
            required_tier,
            trust_class,
            dispatch_owner: dispatch_owner.into(),
            claim_generation,
            attempt,
            next_dispatch_at,
        };
        barrier.validate()?;
        Ok(barrier)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.reason.trim().is_empty()
            || self.project_id.trim().is_empty()
            || self.command_id.trim().is_empty()
            || self.workflow_id.trim().is_empty()
            || self.dispatch_owner.trim().is_empty()
        {
            anyhow::bail!("dispatch barrier identity and explanation fields must not be empty");
        }
        let complete_isolation = self
            .required_tier
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && self
                .trust_class
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());
        match self.reason_code {
            DispatchBarrierReasonCode::IsolationTierUnavailable if !complete_isolation => {
                anyhow::bail!("isolation barrier requires tier and trust class evidence")
            }
            DispatchBarrierReasonCode::RuntimePolicyDisabled
            | DispatchBarrierReasonCode::WorkflowConfigInvalid
                if self.required_tier.is_some() || self.trust_class.is_some() =>
            {
                anyhow::bail!("non-isolation barrier must not include isolation evidence")
            }
            _ => Ok(()),
        }
    }
}

impl super::store::WorkflowRuntimeStore {
    pub async fn defer_claimed_command_if_owned(
        &self,
        command_id: &str,
        dispatch_owner: &str,
        dispatch_claim_generation: u64,
        barrier: DispatchBarrierInput,
        now: DateTime<Utc>,
        backoff: DispatchBackoffPolicy,
    ) -> anyhow::Result<DeferClaimedCommandOutcome> {
        use super::{WorkflowCommandStatus, WorkflowInstance};

        let generation = i64::try_from(dispatch_claim_generation)
            .map_err(|_| anyhow::anyhow!("dispatch claim generation exceeds PostgreSQL BIGINT"))?;
        let workflow_id: Option<(String,)> =
            sqlx::query_as("SELECT workflow_id FROM workflow_commands WHERE id = $1")
                .bind(command_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some((workflow_id,)) = workflow_id else {
            anyhow::bail!("workflow command not found: {command_id}");
        };

        let mut tx = self.pool.begin().await?;
        let workflow_data: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
                .bind(&workflow_id)
                .fetch_optional(&mut *tx)
                .await?;
        let workflow = workflow_data
            .map(|(data,)| serde_json::from_str::<WorkflowInstance>(&data))
            .transpose()?;
        let row: Option<ClaimedCommandRow> = sqlx::query_as(
            "SELECT status, dispatch_owner, dispatch_claim_generation,
                    dispatch_attempt_count, dispatch_barrier::text
             FROM workflow_commands WHERE id = $1 FOR UPDATE",
        )
        .bind(command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status, current_owner, current_generation, attempt_count, persisted_barrier)) =
            row
        else {
            anyhow::bail!("workflow command not found: {command_id}");
        };
        let status = WorkflowCommandStatus::try_from(status.as_str())?;

        if status == WorkflowCommandStatus::Deferred && current_generation == generation {
            let persisted: DispatchBarrier = persisted_barrier
                .ok_or_else(|| anyhow::anyhow!("deferred command is missing barrier evidence"))
                .and_then(|data| serde_json::from_str(&data).map_err(Into::into))?;
            persisted.validate()?;
            if persisted.dispatch_owner == dispatch_owner
                && persisted.claim_generation == dispatch_claim_generation
            {
                tx.rollback().await?;
                return Ok(DeferClaimedCommandOutcome::AlreadyDeferred(persisted));
            }
        }

        if status != WorkflowCommandStatus::Dispatching
            || current_owner.as_deref() != Some(dispatch_owner)
            || current_generation != generation
        {
            tx.rollback().await?;
            return Ok(DeferClaimedCommandOutcome::StaleClaim);
        }

        if let Some(workflow) = workflow.as_ref().filter(|workflow| workflow.is_terminal()) {
            let terminal_status = if workflow.state == "cancelled" {
                WorkflowCommandStatus::Cancelled
            } else {
                WorkflowCommandStatus::Skipped
            };
            sqlx::query(
                "UPDATE workflow_commands
                 SET status = $2, dispatch_owner = NULL,
                     dispatch_lease_expires_at = NULL, dispatch_not_before = NULL,
                     dispatch_barrier = NULL, updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1 AND status = $3 AND dispatch_owner = $4
                   AND dispatch_claim_generation = $5",
            )
            .bind(command_id)
            .bind(terminal_status.as_str())
            .bind(WorkflowCommandStatus::Dispatching.as_str())
            .bind(dispatch_owner)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(DeferClaimedCommandOutcome::WorkflowTerminal {
                status: terminal_status,
            });
        }

        let attempt = u64::try_from(attempt_count)
            .map_err(|_| anyhow::anyhow!("dispatch attempt count is negative"))?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("dispatch attempt count overflow"))?;
        let next_dispatch_at = now
            .checked_add_signed(backoff.delay_for_attempt(attempt))
            .ok_or_else(|| anyhow::anyhow!("next dispatch timestamp overflow"))?;
        let barrier = DispatchBarrier::new(
            barrier.reason_code,
            barrier.reason,
            barrier.project_id,
            command_id,
            &workflow_id,
            barrier.required_tier,
            barrier.trust_class,
            dispatch_owner,
            dispatch_claim_generation,
            attempt,
            next_dispatch_at,
        )?;
        let barrier_json = serde_json::to_string(&barrier)?;
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $2, dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
                 dispatch_not_before = $3, dispatch_attempt_count = $4,
                 dispatch_barrier = $5::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND status = $6 AND dispatch_owner = $7
               AND dispatch_claim_generation = $8",
        )
        .bind(command_id)
        .bind(WorkflowCommandStatus::Deferred.as_str())
        .bind(next_dispatch_at)
        .bind(i64::try_from(attempt)?)
        .bind(&barrier_json)
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(dispatch_owner)
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(DeferClaimedCommandOutcome::StaleClaim);
        }
        super::store::insert_event_tx(
            &mut tx,
            &workflow_id,
            "WorkflowRuntimeDispatchDeferred",
            "workflow_runtime_command_dispatcher",
            serde_json::json!({ "dispatch_barrier": barrier }),
        )
        .await?;
        tx.commit().await?;
        Ok(DeferClaimedCommandOutcome::Deferred(barrier))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_barrier_reason_rejects_mismatched_evidence() {
        let result = DispatchBarrier::new(
            DispatchBarrierReasonCode::RuntimePolicyDisabled,
            "disabled",
            "/project",
            "command",
            "workflow",
            Some("container".to_string()),
            Some("non_collaborator".to_string()),
            "owner",
            1,
            1,
            Utc::now(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn dispatch_barrier_reason_requires_complete_isolation_evidence() {
        let result = DispatchBarrier::new(
            DispatchBarrierReasonCode::IsolationTierUnavailable,
            "unavailable",
            "/project",
            "command",
            "workflow",
            Some("container".to_string()),
            None,
            "owner",
            1,
            1,
            Utc::now(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn deferred_command_backoff_is_positive_exponential_and_capped() {
        let policy = DispatchBackoffPolicy::from_seconds(5, 20).expect("valid backoff");
        assert_eq!(policy.delay_for_attempt(1), chrono::Duration::seconds(5));
        assert_eq!(policy.delay_for_attempt(2), chrono::Duration::seconds(10));
        assert_eq!(policy.delay_for_attempt(3), chrono::Duration::seconds(20));
        assert_eq!(
            policy.delay_for_attempt(u64::MAX),
            chrono::Duration::seconds(20)
        );
    }
}
