use harness_core::config::workflow::WorkflowCandidatesPolicy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const CANDIDATE_FANOUT_BUDGET_STRATEGY: &str = "split_runtime_profile_max_turns";
pub const CANDIDATE_FANOUT_MIN_COUNT: u32 = 2;
pub const CANDIDATE_FANOUT_MAX_COUNT: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateFanoutRequest {
    pub candidate_group_id: String,
    pub candidate_count: u32,
    pub trigger_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_per_candidate: Option<u32>,
}

impl CandidateFanoutRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.candidate_group_id.trim().is_empty() {
            anyhow::bail!("candidate_group_id must not be empty");
        }
        if !(CANDIDATE_FANOUT_MIN_COUNT..=CANDIDATE_FANOUT_MAX_COUNT)
            .contains(&self.candidate_count)
        {
            anyhow::bail!(
                "candidate_count must be between {CANDIDATE_FANOUT_MIN_COUNT} and {CANDIDATE_FANOUT_MAX_COUNT}"
            );
        }
        if self.trigger_label.trim().is_empty() {
            anyhow::bail!("trigger_label must not be empty");
        }
        if self.max_turns_per_candidate == Some(0) {
            anyhow::bail!("max_turns_per_candidate must be greater than zero");
        }
        Ok(())
    }

    pub fn candidate_id(&self, candidate_index: u32) -> String {
        format!("{}:c{}", self.candidate_group_id, candidate_index)
    }

    pub fn command_metadata(&self, candidate_index: u32) -> Value {
        let candidate_id = self.candidate_id(candidate_index);
        json!({
            "candidate_group_id": self.candidate_group_id,
            "candidate_id": candidate_id,
            "candidate_index": candidate_index,
            "candidate_count": self.candidate_count,
            "trigger_label": self.trigger_label,
            "budget": {
                "strategy": CANDIDATE_FANOUT_BUDGET_STRATEGY,
                "candidate_count": self.candidate_count,
                "max_turns_per_candidate": self.max_turns_per_candidate,
            },
        })
    }
}

pub fn candidate_fanout_from_policy(
    workflow_id: &str,
    issue_number: u64,
    labels: &[String],
    policy: &WorkflowCandidatesPolicy,
) -> anyhow::Result<Option<CandidateFanoutRequest>> {
    if !policy.enabled {
        return Ok(None);
    }
    let trigger_label = policy.trigger_label.trim();
    if trigger_label.is_empty() {
        anyhow::bail!("candidates.trigger_label must not be empty");
    }
    if !labels.iter().any(|label| label.trim() == trigger_label) {
        return Ok(None);
    }
    if policy.max_turns_per_candidate == Some(0) {
        anyhow::bail!("candidates.max_turns_per_candidate must be greater than zero");
    }
    let request = CandidateFanoutRequest {
        candidate_group_id: format!("{workflow_id}:candidate-group:issue-{issue_number}"),
        candidate_count: policy
            .n
            .clamp(CANDIDATE_FANOUT_MIN_COUNT, CANDIDATE_FANOUT_MAX_COUNT),
        trigger_label: trigger_label.to_string(),
        max_turns_per_candidate: policy.max_turns_per_candidate,
    };
    request.validate()?;
    Ok(Some(request))
}

pub fn candidate_fanout_from_value(
    value: &Value,
) -> anyhow::Result<Option<CandidateFanoutRequest>> {
    let Some(raw) = value.get("candidate_fanout") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let request: CandidateFanoutRequest = serde_json::from_value(raw.clone())?;
    request.validate()?;
    Ok(Some(request))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(enabled: bool, n: u32) -> WorkflowCandidatesPolicy {
        WorkflowCandidatesPolicy {
            enabled,
            n,
            trigger_label: "best-of-n".to_string(),
            max_turns_per_candidate: None,
        }
    }

    #[test]
    fn candidate_fanout_requires_config_and_trigger_label() -> anyhow::Result<()> {
        let labels = vec!["best-of-n".to_string()];

        assert!(candidate_fanout_from_policy("wf-1", 123, &labels, &policy(false, 2))?.is_none());
        assert!(candidate_fanout_from_policy("wf-1", 123, &[], &policy(true, 2))?.is_none());
        assert!(candidate_fanout_from_policy("wf-1", 123, &labels, &policy(true, 2))?.is_some());
        Ok(())
    }

    #[test]
    fn candidate_fanout_clamps_count_and_preserves_budget_override() -> anyhow::Result<()> {
        let labels = vec!["best-of-n".to_string()];
        let mut cfg = policy(true, 9);
        cfg.max_turns_per_candidate = Some(4);

        let request = candidate_fanout_from_policy("wf-1", 123, &labels, &cfg)?
            .expect("label plus enabled config should fan out");

        assert_eq!(request.candidate_count, 3);
        assert_eq!(request.max_turns_per_candidate, Some(4));
        Ok(())
    }

    #[test]
    fn candidate_fanout_command_metadata_is_indexed() -> anyhow::Result<()> {
        let request = CandidateFanoutRequest {
            candidate_group_id: "wf-1:candidate-group:issue-123".to_string(),
            candidate_count: 2,
            trigger_label: "best-of-n".to_string(),
            max_turns_per_candidate: None,
        };

        let metadata = request.command_metadata(2);

        assert_eq!(metadata["candidate_index"], 2);
        assert_eq!(metadata["candidate_count"], 2);
        assert_eq!(
            metadata["candidate_id"],
            "wf-1:candidate-group:issue-123:c2"
        );
        assert_eq!(
            metadata["budget"]["strategy"],
            CANDIDATE_FANOUT_BUDGET_STRATEGY
        );
        Ok(())
    }
}
