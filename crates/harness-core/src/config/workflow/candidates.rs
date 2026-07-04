use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCandidatesPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_candidate_count")]
    pub n: u32,
    #[serde(default = "default_candidate_trigger_label")]
    pub trigger_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns_per_candidate: Option<u32>,
}

impl Default for WorkflowCandidatesPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            n: default_candidate_count(),
            trigger_label: default_candidate_trigger_label(),
            max_turns_per_candidate: None,
        }
    }
}

fn default_candidate_count() -> u32 {
    2
}

fn default_candidate_trigger_label() -> String {
    "best-of-n".to_string()
}
