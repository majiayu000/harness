use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}
