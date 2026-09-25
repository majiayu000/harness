use serde::{Deserialize, Serialize};

/// Server-verified completion evidence policy (GH-1766).
///
/// The enforcement kill switch is deliberately **not** here: it is the
/// deployment-global `workflow.completion_evidence_enforced` on
/// `WorkflowRuntimeConfig`. A second, project-scoped switch is exactly what
/// GH-1815 removed, and `runtime_completion_evidence_enforced` is a reserved
/// key that configuration loading rejects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCompletionPolicy {
    /// Timeout for the server-side quality-gate validation re-run.
    #[serde(default = "default_quality_gate_validation_timeout_secs")]
    pub quality_gate_validation_timeout_secs: u64,
}

impl Default for RuntimeCompletionPolicy {
    fn default() -> Self {
        Self {
            quality_gate_validation_timeout_secs: default_quality_gate_validation_timeout_secs(),
        }
    }
}

fn default_quality_gate_validation_timeout_secs() -> u64 {
    900
}
