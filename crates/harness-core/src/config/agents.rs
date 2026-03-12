use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

impl Default for AgentReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reviewer_agent: String::new(),
            max_rounds: default_max_agent_review_rounds(),
            review_bot_command: default_review_bot_command(),
        }
    }
}

fn default_review_bot_command() -> String {
    "/gemini review".to_string()
}

fn default_max_agent_review_rounds() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeAgentConfig {
    pub cli_path: PathBuf,
    pub default_model: String,
}

impl Default for ClaudeAgentConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("claude"),
            default_model: "sonnet".to_string(),
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
