use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{RuntimeJob, RuntimeJobStatus};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeJobRunningLeaseState {
    ActiveLeased,
    ExpiredLease,
    MissingLease,
}

impl RuntimeJobRunningLeaseState {
    pub fn status_label(self) -> &'static str {
        match self {
            Self::ActiveLeased => "active_leased",
            Self::ExpiredLease => "expired_lease",
            Self::MissingLease => "missing_lease",
        }
    }
}

pub fn runtime_job_running_lease_state_at(
    job: &RuntimeJob,
    now: DateTime<Utc>,
) -> Option<RuntimeJobRunningLeaseState> {
    if job.status != RuntimeJobStatus::Running {
        return None;
    }
    Some(match job.lease.as_ref() {
        Some(lease) if lease.expires_at > now => RuntimeJobRunningLeaseState::ActiveLeased,
        Some(_) => RuntimeJobRunningLeaseState::ExpiredLease,
        None => RuntimeJobRunningLeaseState::MissingLease,
    })
}
