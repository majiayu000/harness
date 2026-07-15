use super::dispatch_barrier::DispatchBarrier;
use super::model::WorkflowCommand;
use super::status::WorkflowCommandStatus;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub(super) type WorkflowCommandRecordRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    i64,
    i64,
    Option<String>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowCommandRecord {
    pub id: String,
    pub workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub status: WorkflowCommandStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_lease_expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_not_before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub dispatch_attempt_count: u64,
    #[serde(default)]
    pub dispatch_claim_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_barrier: Option<DispatchBarrier>,
    pub command: WorkflowCommand,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(super) fn from_row(
    (
        id,
        workflow_id,
        decision_id,
        status,
        dispatch_owner,
        dispatch_lease_expires_at,
        dispatch_not_before,
        dispatch_attempt_count,
        dispatch_claim_generation,
        dispatch_barrier,
        data,
        created_at,
        updated_at,
    ): WorkflowCommandRecordRow,
) -> anyhow::Result<WorkflowCommandRecord> {
    use anyhow::Context;

    let dispatch_barrier: Option<DispatchBarrier> = dispatch_barrier
        .map(|data| serde_json::from_str(&data))
        .transpose()
        .context("workflow command dispatch barrier evidence is invalid")?;
    if let Some(barrier) = dispatch_barrier.as_ref() {
        barrier.validate()?;
    }
    Ok(WorkflowCommandRecord {
        id,
        workflow_id,
        decision_id,
        status: WorkflowCommandStatus::try_from(status.as_str())?,
        dispatch_owner,
        dispatch_lease_expires_at,
        dispatch_not_before,
        dispatch_attempt_count: u64::try_from(dispatch_attempt_count)
            .context("workflow command dispatch attempt count is negative")?,
        dispatch_claim_generation: u64::try_from(dispatch_claim_generation)
            .context("workflow command dispatch claim generation is negative")?,
        dispatch_barrier,
        command: serde_json::from_str(&data)?,
        created_at,
        updated_at,
    })
}
