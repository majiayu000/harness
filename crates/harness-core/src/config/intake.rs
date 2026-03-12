use serde::{Deserialize, Serialize};

/// GitHub Issues intake configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIntakeConfig {
    /// Enable polling GitHub Issues for tasks. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// GitHub repository in "owner/repo" format.
    #[serde(default)]
    pub repo: String,
    /// Issue label to filter on. Default: "harness".
    #[serde(default = "default_intake_label")]
    pub label: String,
    /// Polling interval in seconds. Default: 30.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

fn default_intake_label() -> String {
    "harness".to_string()
}

fn default_poll_interval_secs() -> u64 {
    30
}

impl Default for GitHubIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: String::new(),
            label: default_intake_label(),
            poll_interval_secs: default_poll_interval_secs(),
        }
    }
}

/// Feishu (飞书) Bot intake configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuIntakeConfig {
    /// Enable Feishu bot webhook. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Feishu app_id. Falls back to $FEISHU_APP_ID env var.
    pub app_id: Option<String>,
    /// Feishu app_secret. Falls back to $FEISHU_APP_SECRET env var.
    pub app_secret: Option<String>,
    /// Webhook verification token for challenge handshake.
    pub verification_token: Option<String>,
    /// Keyword in message text that triggers task creation. Default: "harness".
    #[serde(default = "default_feishu_trigger_keyword")]
    pub trigger_keyword: String,
    /// Default repository to work on (e.g. "owner/repo").
    pub default_repo: Option<String>,
}

fn default_feishu_trigger_keyword() -> String {
    "harness".to_string()
}

impl Default for FeishuIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: None,
            app_secret: None,
            verification_token: None,
            trigger_keyword: default_feishu_trigger_keyword(),
            default_repo: None,
        }
    }
}

/// Multi-channel task intake configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntakeConfig {
    /// GitHub Issues poller configuration.
    #[serde(default)]
    pub github: Option<GitHubIntakeConfig>,
    /// Feishu Bot webhook configuration.
    #[serde(default)]
    pub feishu: Option<FeishuIntakeConfig>,
}
