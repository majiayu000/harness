use crate::types::ReasoningBudget;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Preset capability profile that determines which tools an agent may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityProfile {
    /// Read-only access: Read, Grep, Glob only. Suitable for GC/review agents.
    ReadOnly,
    /// Standard access: Read, Write, Edit, Bash. Suitable for implementation agents.
    Standard,
    /// Full access — all tools. Default profile.
    #[default]
    Full,
}

impl CapabilityProfile {
    /// Returns the explicit tool list for this profile, or `None` for `Full`
    /// (meaning no restriction is applied to the CLI invocation).
    pub fn tools(self) -> Option<Vec<String>> {
        match self {
            CapabilityProfile::ReadOnly => Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
            ]),
            CapabilityProfile::Standard => Some(vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
            ]),
            CapabilityProfile::Full => None,
        }
    }

    /// Human-readable description injected into the agent prompt as context.
    pub fn prompt_note(self) -> Option<&'static str> {
        match self {
            CapabilityProfile::ReadOnly => Some(
                "Tool restriction: you are operating in read-only mode. \
                 Only Read, Grep, and Glob are permitted. \
                 Do NOT call Write, Edit, Bash, or any other tool.",
            ),
            CapabilityProfile::Standard => Some(
                "Tool restriction: you are operating in standard mode. \
                 Only Read, Write, Edit, and Bash are permitted. \
                 Do NOT call tools outside this list.",
            ),
            CapabilityProfile::Full => None,
        }
    }
}

/// Controls how much autonomy the agent has when executing tasks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Agent suggests changes but does not apply them.
    Suggest,
    /// Agent can edit files but requires human approval for shell commands.
    #[default]
    AutoEdit,
    /// Agent has full autonomy — no approval gates.
    FullAuto,
}

/// OS-level sandbox mode for agent subprocess execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    #[default]
    DangerFullAccess,
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    pub default_agent: String,
    pub claude: ClaudeAgentConfig,
    pub codex: CodexAgentConfig,
    pub anthropic_api: AnthropicApiConfig,
    #[serde(default)]
    pub review: AgentReviewConfig,
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
    #[serde(default)]
    pub sandbox_mode: SandboxMode,
    /// Preset capability profile applied to all agents unless overridden.
    #[serde(default)]
    pub capability_profile: CapabilityProfile,
    /// Explicit tool list. When set, takes precedence over `capability_profile`.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Maximum seconds of silence on a stream before declaring a zombie and
    /// killing the subprocess. Set to `0` to disable the per-line idle timeout.
    /// Default: 1800 (30 minutes).
    #[serde(
        default = "default_stream_timeout_secs",
        deserialize_with = "deserialize_stream_timeout_secs"
    )]
    pub stream_timeout_secs: Option<u64>,
}

fn default_stream_timeout_secs() -> Option<u64> {
    Some(1800)
}

/// Deserializer for `stream_timeout_secs`: TOML integer `0` maps to `None`
/// (timeout disabled); any positive value maps to `Some(n)`.
fn deserialize_stream_timeout_secs<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let n = u64::deserialize(deserializer)?;
    Ok(if n == 0 { None } else { Some(n) })
}

impl AgentsConfig {
    /// Resolve the effective tool list from `allowed_tools` (explicit override)
    /// or `capability_profile` (preset). Returns `None` when no restriction applies.
    pub fn resolve_allowed_tools(&self) -> Option<Vec<String>> {
        if let Some(tools) = &self.allowed_tools {
            return Some(tools.clone());
        }
        self.capability_profile.tools()
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default_agent: "claude".to_string(),
            claude: ClaudeAgentConfig::default(),
            codex: CodexAgentConfig::default(),
            anthropic_api: AnthropicApiConfig::default(),
            review: AgentReviewConfig::default(),
            approval_policy: ApprovalPolicy::default(),
            sandbox_mode: SandboxMode::default(),
            capability_profile: CapabilityProfile::default(),
            allowed_tools: None,
            stream_timeout_secs: default_stream_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReviewConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Agent name to use as reviewer. Empty string = auto-select.
    #[serde(default)]
    pub reviewer_agent: String,
    #[serde(default = "default_max_agent_review_rounds")]
    pub max_rounds: u32,
    /// Command to trigger review bot re-review (e.g. "/gemini review", "/reviewbot run").
    #[serde(default = "default_review_bot_command")]
    pub review_bot_command: String,
    /// GitHub login of the external review bot used to verify re-reviews after a fix commit.
    /// Must match the `user.login` field returned by the GitHub reviews API.
    /// Default: "gemini-code-assist[bot]".
    #[serde(default = "default_reviewer_name")]
    pub reviewer_name: String,
    /// Automatically post review_bot_command as a PR comment when a task completes with a PR.
    #[serde(default = "default_review_bot_auto_trigger")]
    pub review_bot_auto_trigger: bool,
}

impl Default for AgentReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reviewer_agent: String::new(),
            max_rounds: default_max_agent_review_rounds(),
            review_bot_command: default_review_bot_command(),
            reviewer_name: default_reviewer_name(),
            review_bot_auto_trigger: default_review_bot_auto_trigger(),
        }
    }
}

fn default_review_bot_command() -> String {
    "/gemini review".to_string()
}

fn default_reviewer_name() -> String {
    "gemini-code-assist[bot]".to_string()
}

fn default_review_bot_auto_trigger() -> bool {
    true
}

fn default_max_agent_review_rounds() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeAgentConfig {
    pub cli_path: PathBuf,
    pub default_model: String,
    /// Optional per-phase model selection. When set, the agent switches models
    /// based on the `execution_phase` field of each `AgentRequest`.
    #[serde(default)]
    pub reasoning_budget: Option<ReasoningBudget>,
}

impl Default for ClaudeAgentConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("claude"),
            default_model: "sonnet".to_string(),
            reasoning_budget: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAgentConfig {
    pub cli_path: PathBuf,
    #[serde(default)]
    pub cloud: CodexCloudConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCloudConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_codex_cloud_cache_ttl_hours")]
    pub cache_ttl_hours: u64,
    #[serde(default)]
    pub setup_commands: Vec<String>,
    #[serde(default)]
    pub setup_secret_env: Vec<String>,
}

impl Default for CodexCloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cache_ttl_hours: default_codex_cloud_cache_ttl_hours(),
            setup_commands: Vec::new(),
            setup_secret_env: Vec::new(),
        }
    }
}

fn default_codex_cloud_cache_ttl_hours() -> u64 {
    12
}

impl Default for CodexAgentConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("codex"),
            cloud: CodexCloudConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicApiConfig {
    pub base_url: String,
    pub default_model: String,
    #[serde(default = "default_anthropic_api_max_tokens")]
    pub max_tokens: u32,
}

fn default_anthropic_api_max_tokens() -> u32 {
    4096
}

impl Default for AnthropicApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            default_model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: default_anthropic_api_max_tokens(),
        }
    }
}
