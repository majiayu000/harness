use serde::{Deserialize, Serialize};

/// Input-fact envelope schemas the runtime can construct and enforce today.
/// An `agent_contract` naming any other id fails definition compilation, so a
/// contract can never declare an input the runtime would silently skip.
pub const SUPPORTED_AGENT_CONTRACT_INPUT_SCHEMAS: &[&str] = &["harness.semantic_activity_input.v1"];

/// Structured-output schemas the runtime can validate today. Same fail-closed
/// rule as the input registry.
pub const SUPPORTED_AGENT_CONTRACT_OUTPUT_SCHEMAS: &[&str] = &["harness.semantic_verdict.v1"];

/// Server ceiling for primary attempts of one agent-contract activity.
pub const AGENT_CONTRACT_MAX_PRIMARY_ATTEMPTS_CEILING: u32 = 2;

/// Server ceiling for structured-output correction turns per primary attempt.
pub const AGENT_CONTRACT_MAX_CORRECTIONS_CEILING: u32 = 1;

/// Tool access an agent-contract activity may declare. Only `none` is
/// supported: the initial contract family is read-nothing semantic judgment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentContractToolPolicy {
    None,
}

/// Whether the activity may mutate anything outside its own reply.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentContractMutationPolicy {
    Forbidden,
}

/// Workspace the runtime prepares for the activity. `ephemeral_empty` never
/// exposes a repository checkout; facts must arrive through the input envelope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentContractWorkspacePolicy {
    EphemeralEmpty,
}

/// Generic execution contract for one Workflow-declared agent activity.
///
/// The Workflow owns schemas, outcomes, and routes; the runtime owns
/// enforcement. The resolved contract participates in the pinned definition
/// identity, so changing any field produces a new definition hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAgentContract {
    pub input_schema: String,
    pub output_schema: String,
    /// Exact outcome vocabulary. The declaring state's `on_signal` routes must
    /// cover exactly this set; the runtime rejects any other outcome value.
    pub allowed_outcomes: Vec<String>,
    pub tools: AgentContractToolPolicy,
    pub mutation: AgentContractMutationPolicy,
    pub workspace: AgentContractWorkspacePolicy,
    /// Must be `true`: the activity always runs in a fresh context with no
    /// inherited conversation.
    pub fresh_context: bool,
    #[serde(default = "default_max_primary_attempts")]
    pub max_primary_attempts: u32,
    #[serde(default = "default_max_corrections")]
    pub max_corrections: u32,
}

fn default_max_primary_attempts() -> u32 {
    1
}

fn default_max_corrections() -> u32 {
    1
}

impl WorkflowAgentContract {
    pub fn validate(&self, activity: &str) -> anyhow::Result<()> {
        if !SUPPORTED_AGENT_CONTRACT_INPUT_SCHEMAS.contains(&self.input_schema.as_str()) {
            anyhow::bail!(
                "activity '{activity}' agent_contract input_schema '{}' is not supported; supported input schemas: {}",
                self.input_schema,
                SUPPORTED_AGENT_CONTRACT_INPUT_SCHEMAS.join(", ")
            );
        }
        if !SUPPORTED_AGENT_CONTRACT_OUTPUT_SCHEMAS.contains(&self.output_schema.as_str()) {
            anyhow::bail!(
                "activity '{activity}' agent_contract output_schema '{}' is not supported; supported output schemas: {}",
                self.output_schema,
                SUPPORTED_AGENT_CONTRACT_OUTPUT_SCHEMAS.join(", ")
            );
        }
        if self.allowed_outcomes.is_empty() {
            anyhow::bail!(
                "activity '{activity}' agent_contract must declare at least one allowed outcome"
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for outcome in &self.allowed_outcomes {
            if outcome.is_empty()
                || outcome.trim() != outcome
                || outcome.chars().any(char::is_whitespace)
            {
                anyhow::bail!(
                    "activity '{activity}' agent_contract outcome '{outcome}' must be a non-empty token without whitespace"
                );
            }
            if !seen.insert(outcome.as_str()) {
                anyhow::bail!(
                    "activity '{activity}' agent_contract repeats allowed outcome '{outcome}'"
                );
            }
        }
        if !self.fresh_context {
            anyhow::bail!(
                "activity '{activity}' agent_contract requires fresh_context: true; inherited context is not supported"
            );
        }
        if self.max_primary_attempts == 0
            || self.max_primary_attempts > AGENT_CONTRACT_MAX_PRIMARY_ATTEMPTS_CEILING
        {
            anyhow::bail!(
                "activity '{activity}' agent_contract max_primary_attempts {} must be between 1 and {}",
                self.max_primary_attempts,
                AGENT_CONTRACT_MAX_PRIMARY_ATTEMPTS_CEILING
            );
        }
        if self.max_corrections > AGENT_CONTRACT_MAX_CORRECTIONS_CEILING {
            anyhow::bail!(
                "activity '{activity}' agent_contract max_corrections {} exceeds the server ceiling {}",
                self.max_corrections,
                AGENT_CONTRACT_MAX_CORRECTIONS_CEILING
            );
        }
        Ok(())
    }
}
