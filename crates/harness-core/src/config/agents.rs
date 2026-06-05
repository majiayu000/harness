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
    ReadOnlyWithNetwork,
    WorkspaceWrite,
    #[default]
    DangerFullAccess,
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::ReadOnlyWithNetwork => "read-only-with-network",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// Default agent name. Use `"auto"` to select the first registered agent.
    #[serde(default = "default_default_agent")]
    pub default_agent: String,
    /// Optional priority list for complex/critical task routing.
    /// Example: `["codex", "claude"]`.
    #[serde(default)]
    pub complexity_preferred_agents: Vec<String>,
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
    /// Default: 3600 (60 minutes).
    #[serde(
        default = "default_stream_timeout_secs",
        deserialize_with = "deserialize_stream_timeout_secs"
    )]
    pub stream_timeout_secs: Option<u64>,
}

fn default_stream_timeout_secs() -> Option<u64> {
    Some(3600)
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
            default_agent: default_default_agent(),
            complexity_preferred_agents: Vec::new(),
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

fn default_default_agent() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStrategy {
    #[default]
    LocalFirst,
    LegacyHostedBot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCliReviewConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_codex_cli_review_path")]
    pub cli_path: PathBuf,
    #[serde(default = "default_codex_model")]
    pub model: String,
    #[serde(default = "default_codex_reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(default = "default_codex_cli_review_base_ref")]
    pub base_ref: String,
    #[serde(default = "default_codex_cli_review_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_codex_cli_review_output_format")]
    pub output_format: String,
}

impl Default for CodexCliReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cli_path: default_codex_cli_review_path(),
            model: default_codex_model(),
            reasoning_effort: default_codex_reasoning_effort(),
            base_ref: default_codex_cli_review_base_ref(),
            timeout_secs: default_codex_cli_review_timeout_secs(),
            output_format: default_codex_cli_review_output_format(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAgentReviewProviderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_reviewer_agent")]
    pub reviewer_agent: String,
    #[serde(default = "default_max_agent_review_rounds")]
    pub max_rounds: u32,
}

impl Default for CodexAgentReviewProviderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reviewer_agent: default_reviewer_agent(),
            max_rounds: default_max_agent_review_rounds(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawCodexAgentReviewProviderConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    reviewer_agent: Option<String>,
    #[serde(default)]
    max_rounds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalBotReviewProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub trigger_command: String,
    #[serde(default)]
    pub reviewer_name: String,
    #[serde(default)]
    pub auto_trigger: bool,
    #[serde(default = "default_external_bot_wait_secs")]
    pub wait_secs: u64,
    #[serde(default = "default_max_agent_review_rounds")]
    pub max_rounds: u32,
}

impl ExternalBotReviewProviderConfig {
    fn gemini(trigger_command: String, reviewer_name: String, auto_trigger: bool) -> Self {
        Self {
            enabled: true,
            trigger_command,
            reviewer_name,
            auto_trigger,
            wait_secs: default_external_bot_wait_secs(),
            max_rounds: default_max_agent_review_rounds(),
        }
    }

    fn codex(auto_trigger: bool) -> Self {
        Self {
            enabled: true,
            trigger_command: "@codex".to_string(),
            reviewer_name: "chatgpt-codex-connector[bot]".to_string(),
            auto_trigger,
            wait_secs: default_external_bot_wait_secs(),
            max_rounds: default_max_agent_review_rounds(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawExternalBotReviewProviderConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    trigger_command: Option<String>,
    #[serde(default)]
    reviewer_name: Option<String>,
    #[serde(default)]
    auto_trigger: Option<bool>,
    #[serde(default)]
    wait_secs: Option<u64>,
    #[serde(default)]
    max_rounds: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentReviewConfig {
    pub enabled: bool,
    pub strategy: ReviewStrategy,
    pub required_providers: Vec<String>,
    pub advisory_providers: Vec<String>,
    pub external_required: bool,
    pub publish_local_review_comment: bool,
    pub codex_cli_review: CodexCliReviewConfig,
    pub codex_agent_review: CodexAgentReviewProviderConfig,
    pub gemini_github_bot: ExternalBotReviewProviderConfig,
    pub codex_github_bot: ExternalBotReviewProviderConfig,
    /// Agent name to use as reviewer. Empty string = auto-select.
    pub reviewer_agent: String,
    pub max_rounds: u32,
    /// Command to trigger review bot re-review (e.g. "/gemini review", "/reviewbot run").
    pub review_bot_command: String,
    /// Automatically post review_bot_command as a PR comment when a task completes with a PR.
    /// Disabled by default so local agent review is the primary gate and hosted review
    /// bots remain advisory opt-ins.
    pub review_bot_auto_trigger: bool,
    /// GitHub login of the review bot (used for freshness checks in review loop).
    pub reviewer_name: String,
    /// Ordered review fallback chain, e.g. ["gemini", "codex"].
    pub fallback_chain: Vec<String>,
    /// Consecutive rounds with no new bot activity before silence can graduate.
    pub silence_rounds_threshold: u32,
    /// Minimum minutes after the latest commit before silence can graduate.
    pub silence_min_minutes_after_commit: u32,
}

impl<'de> Deserialize<'de> for AgentReviewConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawAgentReviewConfig {
            #[serde(default)]
            enabled: Option<bool>,
            #[serde(default)]
            strategy: Option<ReviewStrategy>,
            #[serde(default)]
            required_providers: Option<Vec<String>>,
            #[serde(default)]
            advisory_providers: Option<Vec<String>>,
            #[serde(default)]
            external_required: Option<bool>,
            #[serde(default)]
            publish_local_review_comment: Option<bool>,
            #[serde(default)]
            codex_cli_review: Option<CodexCliReviewConfig>,
            #[serde(default)]
            codex_agent_review: Option<RawCodexAgentReviewProviderConfig>,
            #[serde(default)]
            gemini_github_bot: Option<RawExternalBotReviewProviderConfig>,
            #[serde(default)]
            codex_github_bot: Option<RawExternalBotReviewProviderConfig>,
            #[serde(default)]
            reviewer_agent: Option<String>,
            #[serde(default)]
            max_rounds: Option<u32>,
            #[serde(default)]
            review_bot_command: Option<String>,
            #[serde(default)]
            review_bot_auto_trigger: Option<bool>,
            #[serde(default)]
            reviewer_name: Option<String>,
            #[serde(default)]
            fallback_chain: Option<Vec<String>>,
            #[serde(default)]
            silence_rounds_threshold: Option<u32>,
            #[serde(default)]
            silence_min_minutes_after_commit: Option<u32>,
        }

        let raw = RawAgentReviewConfig::deserialize(deserializer)?;
        let enabled = raw.enabled.unwrap_or_else(default_agent_review_enabled);
        let review_bot_auto_trigger = raw.review_bot_auto_trigger.unwrap_or(!enabled);
        let review_bot_command = raw
            .review_bot_command
            .unwrap_or_else(default_review_bot_command);
        let reviewer_name = raw.reviewer_name.unwrap_or_else(default_reviewer_name);
        let reviewer_agent = raw.reviewer_agent.unwrap_or_else(default_reviewer_agent);
        let max_rounds = raw
            .max_rounds
            .unwrap_or_else(default_max_agent_review_rounds);
        let fallback_chain = raw
            .fallback_chain
            .unwrap_or_else(default_review_fallback_chain);
        let required_providers = raw
            .required_providers
            .unwrap_or_else(|| default_required_review_providers(enabled));
        let advisory_providers = raw
            .advisory_providers
            .unwrap_or_else(|| normalize_external_bot_chain(&fallback_chain));
        let default_gemini_bot = ExternalBotReviewProviderConfig::gemini(
            review_bot_command.clone(),
            reviewer_name.clone(),
            review_bot_auto_trigger,
        );
        let default_codex_bot = ExternalBotReviewProviderConfig::codex(review_bot_auto_trigger);
        let default_codex_agent = CodexAgentReviewProviderConfig {
            enabled: false,
            reviewer_agent: reviewer_agent.clone(),
            max_rounds,
        };

        Ok(Self {
            enabled,
            strategy: raw.strategy.unwrap_or_default(),
            required_providers,
            advisory_providers,
            external_required: raw.external_required.unwrap_or(false),
            publish_local_review_comment: raw.publish_local_review_comment.unwrap_or(false),
            codex_cli_review: raw.codex_cli_review.unwrap_or_default(),
            codex_agent_review: merge_codex_agent_review_config(
                raw.codex_agent_review,
                default_codex_agent,
            ),
            gemini_github_bot: merge_external_bot_config(raw.gemini_github_bot, default_gemini_bot),
            codex_github_bot: merge_external_bot_config(raw.codex_github_bot, default_codex_bot),
            reviewer_agent,
            max_rounds,
            review_bot_command,
            review_bot_auto_trigger,
            reviewer_name,
            fallback_chain,
            silence_rounds_threshold: raw
                .silence_rounds_threshold
                .unwrap_or_else(default_silence_rounds_threshold),
            silence_min_minutes_after_commit: raw
                .silence_min_minutes_after_commit
                .unwrap_or_else(default_silence_min_minutes_after_commit),
        })
    }
}

impl Default for AgentReviewConfig {
    fn default() -> Self {
        Self {
            enabled: default_agent_review_enabled(),
            strategy: ReviewStrategy::default(),
            required_providers: default_required_review_providers(true),
            advisory_providers: default_advisory_review_providers(),
            external_required: false,
            publish_local_review_comment: false,
            codex_cli_review: CodexCliReviewConfig::default(),
            codex_agent_review: CodexAgentReviewProviderConfig::default(),
            gemini_github_bot: ExternalBotReviewProviderConfig::gemini(
                default_review_bot_command(),
                default_reviewer_name(),
                default_review_bot_auto_trigger(),
            ),
            codex_github_bot: ExternalBotReviewProviderConfig::codex(
                default_review_bot_auto_trigger(),
            ),
            reviewer_agent: default_reviewer_agent(),
            max_rounds: default_max_agent_review_rounds(),
            review_bot_command: default_review_bot_command(),
            review_bot_auto_trigger: default_review_bot_auto_trigger(),
            reviewer_name: default_reviewer_name(),
            fallback_chain: default_review_fallback_chain(),
            silence_rounds_threshold: default_silence_rounds_threshold(),
            silence_min_minutes_after_commit: default_silence_min_minutes_after_commit(),
        }
    }
}

fn default_agent_review_enabled() -> bool {
    true
}

fn default_reviewer_agent() -> String {
    "codex".to_string()
}

fn default_true() -> bool {
    true
}

fn default_reviewer_name() -> String {
    "gemini-code-assist[bot]".to_string()
}

fn default_review_bot_command() -> String {
    "/gemini review".to_string()
}

fn default_review_bot_auto_trigger() -> bool {
    false
}

fn default_max_agent_review_rounds() -> u32 {
    3
}

fn default_required_review_providers(enabled: bool) -> Vec<String> {
    if enabled {
        vec!["codex_cli_review".to_string()]
    } else {
        Vec::new()
    }
}

fn default_advisory_review_providers() -> Vec<String> {
    vec![
        "gemini_github_bot".to_string(),
        "codex_github_bot".to_string(),
    ]
}

fn default_review_fallback_chain() -> Vec<String> {
    vec!["gemini".to_string(), "codex".to_string()]
}

fn normalize_external_bot_chain(chain: &[String]) -> Vec<String> {
    chain
        .iter()
        .filter_map(|provider| {
            let trimmed = provider.trim();
            if trimmed.is_empty() {
                return None;
            }
            let normalized = trimmed.to_ascii_lowercase();
            Some(match normalized.as_str() {
                "gemini" | "gemini_github_bot" => "gemini_github_bot".to_string(),
                "codex" | "codex_github_bot" => "codex_github_bot".to_string(),
                _ => normalized,
            })
        })
        .collect()
}

fn merge_codex_agent_review_config(
    raw: Option<RawCodexAgentReviewProviderConfig>,
    defaults: CodexAgentReviewProviderConfig,
) -> CodexAgentReviewProviderConfig {
    let Some(raw) = raw else {
        return defaults;
    };
    CodexAgentReviewProviderConfig {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        reviewer_agent: raw
            .reviewer_agent
            .filter(|value| !value.is_empty())
            .unwrap_or(defaults.reviewer_agent),
        max_rounds: raw.max_rounds.unwrap_or(defaults.max_rounds),
    }
}

fn merge_external_bot_config(
    raw: Option<RawExternalBotReviewProviderConfig>,
    defaults: ExternalBotReviewProviderConfig,
) -> ExternalBotReviewProviderConfig {
    let Some(raw) = raw else {
        return defaults;
    };
    ExternalBotReviewProviderConfig {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        trigger_command: raw
            .trigger_command
            .filter(|value| !value.is_empty())
            .unwrap_or(defaults.trigger_command),
        reviewer_name: raw
            .reviewer_name
            .filter(|value| !value.is_empty())
            .unwrap_or(defaults.reviewer_name),
        auto_trigger: raw.auto_trigger.unwrap_or(defaults.auto_trigger),
        wait_secs: raw.wait_secs.unwrap_or(defaults.wait_secs),
        max_rounds: raw.max_rounds.unwrap_or(defaults.max_rounds),
    }
}

fn default_codex_cli_review_path() -> PathBuf {
    PathBuf::from("codex")
}

fn default_codex_cli_review_base_ref() -> String {
    "origin/main".to_string()
}

fn default_codex_cli_review_timeout_secs() -> u64 {
    1800
}

fn default_codex_cli_review_output_format() -> String {
    "json".to_string()
}

fn default_external_bot_wait_secs() -> u64 {
    300
}

fn default_silence_rounds_threshold() -> u32 {
    3
}

fn default_silence_min_minutes_after_commit() -> u32 {
    30
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
            reasoning_budget: Some(ReasoningBudget::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAgentConfig {
    pub cli_path: PathBuf,
    #[serde(default = "default_codex_model")]
    pub default_model: String,
    #[serde(default = "default_codex_reasoning_effort")]
    pub reasoning_effort: String,
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

fn default_codex_model() -> String {
    "gpt-5.4".to_string()
}

fn default_codex_reasoning_effort() -> String {
    "high".to_string()
}

impl Default for CodexAgentConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("codex"),
            default_model: default_codex_model(),
            reasoning_effort: default_codex_reasoning_effort(),
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
            default_model: "claude-sonnet-4-6".to_string(),
            max_tokens: default_anthropic_api_max_tokens(),
        }
    }
}

#[cfg(test)]
#[path = "agents_review_tests.rs"]
mod agents_review_tests;
