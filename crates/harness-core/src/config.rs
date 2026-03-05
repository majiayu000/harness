use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub server: ServerConfig,
    pub agents: AgentsConfig,
    pub gc: GcConfig,
    pub rules: RulesConfig,
    pub observe: ObserveConfig,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            agents: AgentsConfig::default(),
            gc: GcConfig::default(),
            rules: RulesConfig::default(),
            observe: ObserveConfig::default(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    pub default_agent: String,
    pub claude: ClaudeAgentConfig,
    pub codex: CodexAgentConfig,
    pub anthropic_api: AnthropicApiConfig,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            default_agent: "claude".to_string(),
            claude: ClaudeAgentConfig::default(),
            codex: CodexAgentConfig::default(),
            anthropic_api: AnthropicApiConfig::default(),
        }
    }
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
}

impl Default for CodexAgentConfig {
    fn default() -> Self {
        Self {
            cli_path: PathBuf::from("codex"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicApiConfig {
    pub base_url: String,
    pub default_model: String,
}

impl Default for AnthropicApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com".to_string(),
            default_model: "claude-sonnet-4-20250514".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    pub max_drafts_per_run: usize,
    pub budget_per_signal_usd: f64,
    pub total_budget_usd: f64,
    pub signal_thresholds: SignalThresholds,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_drafts_per_run: 5,
            budget_per_signal_usd: 0.50,
            total_budget_usd: 5.0,
            signal_thresholds: SignalThresholds::default(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesConfig {
    pub discovery_paths: Vec<PathBuf>,
    pub builtin_path: Option<PathBuf>,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            discovery_paths: vec![],
            builtin_path: None,
        }
    }
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
