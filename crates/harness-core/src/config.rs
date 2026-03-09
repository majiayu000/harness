use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub server: ServerConfig,
    pub agents: AgentsConfig,
    pub gc: GcConfig,
    pub rules: RulesConfig,
    pub observe: ObserveConfig,
    pub otel: OtelConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
}

/// Per-project post-execution validation configuration.
///
/// Commands run after agent output to verify code quality before
/// continuing to the review loop. Empty `pre_commit` triggers language detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// Commands run after each implementation turn (format check, compile, lint).
    #[serde(default)]
    pub pre_commit: Vec<String>,
    /// Commands run before pushing (e.g., full test suite).
    #[serde(default)]
    pub pre_push: Vec<String>,
    /// Timeout in seconds for each individual validation command.
    #[serde(default = "default_validation_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum number of auto-retry attempts on validation failure.
    #[serde(default = "default_validation_max_retries")]
    pub max_retries: u32,
}

fn default_validation_timeout_secs() -> u64 {
    120
}

fn default_validation_max_retries() -> u32 {
    2
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            pre_commit: Vec::new(),
            pre_push: Vec::new(),
            timeout_secs: default_validation_timeout_secs(),
            max_retries: default_validation_max_retries(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub transport: Transport,
    pub http_addr: SocketAddr,
    pub data_dir: PathBuf,
    #[serde(default = "default_project_root")]
    pub project_root: PathBuf,
    #[serde(default)]
    pub github_webhook_secret: Option<String>,
    #[serde(default = "default_notification_broadcast_capacity")]
    pub notification_broadcast_capacity: usize,
    #[serde(default = "default_notification_lag_log_every")]
    pub notification_lag_log_every: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
            http_addr: SocketAddr::from(([127, 0, 0, 1], 9800)),
            data_dir: dirs_data_dir().join("harness"),
            project_root: default_project_root(),
            github_webhook_secret: None,
            notification_broadcast_capacity: default_notification_broadcast_capacity(),
            notification_lag_log_every: default_notification_lag_log_every(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
    WebSocket,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    pub max_drafts_per_run: usize,
    pub budget_per_signal_usd: f64,
    pub total_budget_usd: f64,
    #[serde(default = "default_gc_adopt_wait_secs")]
    pub adopt_wait_secs: u64,
    #[serde(default = "default_gc_adopt_max_rounds")]
    pub adopt_max_rounds: u32,
    #[serde(default = "default_gc_adopt_turn_timeout_secs")]
    pub adopt_turn_timeout_secs: u64,
    pub signal_thresholds: SignalThresholds,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_drafts_per_run: 5,
            budget_per_signal_usd: 0.50,
            total_budget_usd: 5.0,
            adopt_wait_secs: default_gc_adopt_wait_secs(),
            adopt_max_rounds: default_gc_adopt_max_rounds(),
            adopt_turn_timeout_secs: default_gc_adopt_turn_timeout_secs(),
            signal_thresholds: SignalThresholds::default(),
        }
    }
}

fn default_gc_adopt_wait_secs() -> u64 {
    120
}

fn default_gc_adopt_max_rounds() -> u32 {
    3
}

fn default_gc_adopt_turn_timeout_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalThresholds {
    pub repeated_warn_min: usize,
    pub chronic_block_min: usize,
    pub hot_file_edits_min: usize,
    pub slow_op_threshold_ms: u64,
    pub slow_op_count_min: usize,
    pub escalation_ratio: f64,
    pub violation_min: usize,
}

impl Default for SignalThresholds {
    fn default() -> Self {
        Self {
            repeated_warn_min: 10,
            chronic_block_min: 5,
            hot_file_edits_min: 20,
            slow_op_threshold_ms: 5000,
            slow_op_count_min: 10,
            escalation_ratio: 1.5,
            violation_min: 5,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RulesConfig {
    #[serde(default)]
    pub discovery_paths: Vec<PathBuf>,
    #[serde(default)]
    pub builtin_path: Option<PathBuf>,
    #[serde(default)]
    pub exec_policy_paths: Vec<PathBuf>,
    #[serde(default)]
    pub requirements_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveConfig {
    pub db_path: PathBuf,
    pub session_renewal_secs: u64,
    pub log_retention_days: u32,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            db_path: dirs_data_dir().join("harness").join("harness.db"),
            session_renewal_secs: 1800,
            log_retention_days: 90,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelConfig {
    #[serde(default = "default_otel_environment")]
    pub environment: String,
    #[serde(default)]
    pub exporter: OtelExporter,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub log_user_prompt: bool,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            environment: default_otel_environment(),
            exporter: OtelExporter::default(),
            endpoint: None,
            log_user_prompt: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OtelExporter {
    #[default]
    Disabled,
    OtlpHttp,
    OtlpGrpc,
}

fn default_otel_environment() -> String {
    "development".to_string()
}

fn dirs_data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn default_project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn default_notification_broadcast_capacity() -> usize {
    256
}

fn default_notification_lag_log_every() -> u64 {
    1
}

mod dirs {
    use std::path::PathBuf;

    pub fn data_local_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join("Library/Application Support"))
        }
        #[cfg(target_os = "linux")]
        {
            std::env::var("XDG_DATA_HOME")
                .ok()
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .map(|h| PathBuf::from(h).join(".local/share"))
                })
        }
        #[cfg(target_os = "windows")]
        {
            std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_review_config_defaults() {
        let config = AgentReviewConfig::default();
        assert!(!config.enabled);
        assert!(config.reviewer_agent.is_empty());
        assert_eq!(config.max_rounds, 3);
    }

    #[test]
    fn agent_review_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            reviewer_agent = "codex"
            max_rounds = 5
        "#;
        let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.reviewer_agent, "codex");
        assert_eq!(config.max_rounds, 5);
    }

    #[test]
    fn agent_review_config_deserializes_with_defaults() {
        let toml_str = r#"
            enabled = true
        "#;
        let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!(config.reviewer_agent.is_empty());
        assert_eq!(config.max_rounds, 3);
    }

    #[test]
    fn agents_config_includes_review() {
        let toml_str = r#"
            default_agent = "claude"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"
            [review]
            enabled = true
            reviewer_agent = "codex"
            max_rounds = 2
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.review.enabled);
        assert_eq!(config.review.reviewer_agent, "codex");
        assert_eq!(config.review.max_rounds, 2);
        assert_eq!(
            config.anthropic_api.max_tokens,
            default_anthropic_api_max_tokens()
        );
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn anthropic_api_config_deserializes_configured_max_tokens() {
        let toml_str = r#"
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"
            max_tokens = 8192
        "#;
        let config: AnthropicApiConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_tokens, 8192);
    }

    #[test]
    fn approval_policy_defaults_to_auto_edit() {
        let config = AgentsConfig::default();
        assert_eq!(config.approval_policy, ApprovalPolicy::AutoEdit);
    }

    #[test]
    fn sandbox_mode_defaults_to_danger_full_access() {
        let config = AgentsConfig::default();
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn approval_policy_deserializes_from_toml() {
        let toml_str = r#"
            default_agent = "claude"
            approval_policy = "full_auto"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.approval_policy, ApprovalPolicy::FullAuto);
    }

    #[test]
    fn sandbox_mode_deserializes_from_toml() {
        let toml_str = r#"
            default_agent = "claude"
            sandbox_mode = "danger-full-access"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn anthropic_api_config_defaults_max_tokens_when_missing() {
        let toml_str = r#"
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"
        "#;
        let config: AnthropicApiConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_tokens, default_anthropic_api_max_tokens());
    }

    #[test]
    fn codex_cloud_config_defaults() {
        let config = CodexCloudConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.cache_ttl_hours, 12);
        assert!(config.setup_commands.is_empty());
        assert!(config.setup_secret_env.is_empty());
    }

    #[test]
    fn codex_agent_config_deserializes_cloud_block() {
        let toml_str = r#"
            cli_path = "codex"
            [cloud]
            enabled = true
            cache_ttl_hours = 6
            setup_commands = ["npm ci", "cargo fetch"]
            setup_secret_env = ["NPM_TOKEN"]
        "#;
        let config: CodexAgentConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cloud.enabled);
        assert_eq!(config.cloud.cache_ttl_hours, 6);
        assert_eq!(
            config.cloud.setup_commands,
            vec!["npm ci".to_string(), "cargo fetch".to_string()]
        );
        assert_eq!(config.cloud.setup_secret_env, vec!["NPM_TOKEN".to_string()]);
    }

    #[test]
    fn rules_config_defaults_do_not_autoload_requirements() {
        let rules = RulesConfig::default();
        assert!(rules.exec_policy_paths.is_empty());
        assert_eq!(rules.requirements_path, None);
    }

    #[test]
    fn rules_config_deserializes_when_new_fields_are_missing() {
        let toml_str = r#"
            discovery_paths = []
        "#;
        let rules: RulesConfig = toml::from_str(toml_str).unwrap();
        assert!(rules.exec_policy_paths.is_empty());
        assert_eq!(rules.requirements_path, None);
    }

    #[test]
    fn otel_config_defaults_to_disabled_exporter() {
        let config = OtelConfig::default();
        assert_eq!(config.exporter, OtelExporter::Disabled);
        assert!(config.endpoint.is_none());
        assert!(!config.log_user_prompt);
        assert_eq!(config.environment, "development");
    }

    #[test]
    fn harness_config_deserializes_otel_section() {
        let toml_str = r#"
            [server]
            transport = "stdio"
            http_addr = "127.0.0.1:9800"
            data_dir = "."
            project_root = "."

            [agents]
            default_agent = "claude"
            [agents.claude]
            cli_path = "claude"
            default_model = "sonnet"
            [agents.codex]
            cli_path = "codex"
            [agents.anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-20250514"

            [gc]
            max_drafts_per_run = 5
            budget_per_signal_usd = 0.5
            total_budget_usd = 5.0
            [gc.signal_thresholds]
            repeated_warn_min = 10
            chronic_block_min = 5
            hot_file_edits_min = 20
            slow_op_threshold_ms = 5000
            slow_op_count_min = 10
            escalation_ratio = 1.5
            violation_min = 5

            [rules]
            discovery_paths = []

            [observe]
            db_path = "."
            session_renewal_secs = 1800
            log_retention_days = 90

            [otel]
            environment = "staging"
            exporter = "otlp-http"
            endpoint = "http://collector:4318"
            log_user_prompt = true
        "#;

        let config: HarnessConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.otel.environment, "staging");
        assert_eq!(config.otel.exporter, OtelExporter::OtlpHttp);
        assert_eq!(
            config.otel.endpoint.as_deref(),
            Some("http://collector:4318")
        );
        assert!(config.otel.log_user_prompt);
    }

    #[test]
    fn server_config_webhook_secret_defaults_to_none() {
        let config = ServerConfig::default();
        assert!(config.github_webhook_secret.is_none());
    }

    #[test]
    fn server_config_deserializes_webhook_secret() {
        let toml_str = r#"
            transport = "http"
            http_addr = "127.0.0.1:9800"
            data_dir = "/tmp/harness"
            project_root = "."
            github_webhook_secret = "super-secret"
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.github_webhook_secret.as_deref(),
            Some("super-secret")
        );
    }

    #[derive(Deserialize)]
    struct TransportWrapper {
        t: Transport,
    }

    #[test]
    fn transport_deserializes_all_variants() {
        let parse = |s: &str| -> Transport {
            toml::from_str::<TransportWrapper>(&format!("t = \"{s}\""))
                .unwrap_or_else(|e| panic!("failed to parse transport `{s}`: {e}"))
                .t
        };
        assert_eq!(parse("stdio"), Transport::Stdio);
        assert_eq!(parse("http"), Transport::Http);
        assert_eq!(parse("web_socket"), Transport::WebSocket);
    }

    #[test]
    fn transport_rejects_unknown_variant() {
        let result = toml::from_str::<TransportWrapper>(r#"t = "grpc""#);
        assert!(result.is_err());
    }

    #[test]
    fn gc_config_defaults_are_consistent() {
        let config = GcConfig::default();
        assert_eq!(config.max_drafts_per_run, 5);
        assert_eq!(config.adopt_wait_secs, 120);
        assert_eq!(config.adopt_max_rounds, 3);
        assert_eq!(config.adopt_turn_timeout_secs, 600);
        assert!(config.budget_per_signal_usd > 0.0);
        assert!(config.total_budget_usd >= config.budget_per_signal_usd);
    }

    #[test]
    fn gc_config_deserializes_from_toml() {
        let toml_str = r#"
            max_drafts_per_run = 10
            budget_per_signal_usd = 1.0
            total_budget_usd = 20.0
            adopt_wait_secs = 60
            adopt_max_rounds = 5
            adopt_turn_timeout_secs = 300

            [signal_thresholds]
            repeated_warn_min = 20
            chronic_block_min = 10
            hot_file_edits_min = 30
            slow_op_threshold_ms = 3000
            slow_op_count_min = 5
            escalation_ratio = 2.0
            violation_min = 3
        "#;
        let config: GcConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_drafts_per_run, 10);
        assert_eq!(config.adopt_wait_secs, 60);
        assert_eq!(config.signal_thresholds.repeated_warn_min, 20);
        assert_eq!(config.signal_thresholds.escalation_ratio, 2.0);
    }

    #[test]
    fn signal_thresholds_defaults_are_consistent() {
        let thresholds = SignalThresholds::default();
        assert_eq!(thresholds.repeated_warn_min, 10);
        assert_eq!(thresholds.chronic_block_min, 5);
        assert_eq!(thresholds.hot_file_edits_min, 20);
        assert_eq!(thresholds.slow_op_threshold_ms, 5000);
        assert_eq!(thresholds.slow_op_count_min, 10);
        assert_eq!(thresholds.violation_min, 5);
        assert!(thresholds.escalation_ratio > 1.0);
    }

    #[test]
    fn harness_config_default_roundtrip() {
        let config = HarnessConfig::default();
        assert_eq!(config.agents.default_agent, "claude");
        assert_eq!(config.server.transport, Transport::Stdio);
        assert!(!config.agents.review.enabled);
        assert_eq!(config.otel.exporter, OtelExporter::Disabled);
        assert_eq!(config.gc.max_drafts_per_run, 5);
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let result = toml::from_str::<HarnessConfig>("not valid toml {{{}}}");
        assert!(result.is_err());
    }

    #[test]
    fn otel_exporter_grpc_variant_deserializes() {
        let toml_str = r#"
            environment = "production"
            exporter = "otlp-grpc"
            endpoint = "http://otel-collector:4317"
        "#;
        let config: OtelConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.exporter, OtelExporter::OtlpGrpc);
        assert_eq!(
            config.endpoint.as_deref(),
            Some("http://otel-collector:4317")
        );
    }

    #[test]
    fn agent_review_config_review_bot_command_default() {
        let config = AgentReviewConfig::default();
        assert_eq!(config.review_bot_command, "/gemini review");
    }

    #[test]
    fn server_config_notification_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.notification_broadcast_capacity, 256);
        assert_eq!(config.notification_lag_log_every, 1);
    }

    #[test]
    fn validation_config_defaults() {
        let config = ValidationConfig::default();
        assert!(config.pre_commit.is_empty());
        assert!(config.pre_push.is_empty());
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn validation_config_deserializes_from_toml() {
        let toml_str = r#"
            pre_commit = ["cargo fmt --all -- --check", "cargo check"]
            pre_push = ["cargo test"]
            timeout_secs = 60
            max_retries = 3
        "#;
        let config: ValidationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.pre_commit.len(), 2);
        assert_eq!(config.pre_push, vec!["cargo test"]);
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn validation_config_deserializes_with_defaults() {
        let toml_str = r#"
            pre_commit = ["ruff check ."]
        "#;
        let config: ValidationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.pre_commit, vec!["ruff check ."]);
        assert!(config.pre_push.is_empty());
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.max_retries, 2);
    }
}
