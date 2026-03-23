use std::fmt;

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
#[derive(Clone, Serialize, Deserialize)]
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

impl fmt::Debug for FeishuIntakeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeishuIntakeConfig")
            .field("enabled", &self.enabled)
            .field("app_id", &self.app_id.as_ref().map(|_| "[REDACTED]"))
            .field(
                "app_secret",
                &self.app_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "verification_token",
                &self.verification_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("trigger_keyword", &self.trigger_keyword)
            .field("default_repo", &self.default_repo)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feishu_debug_redacts_secrets() {
        let config = FeishuIntakeConfig {
            enabled: true,
            app_id: Some("app-123".to_string()),
            app_secret: Some("super-secret".to_string()),
            verification_token: Some("tok-abc".to_string()),
            trigger_keyword: "harness".to_string(),
            default_repo: Some("owner/repo".to_string()),
        };
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("app-123"),
            "app_id must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("super-secret"),
            "app_secret must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("tok-abc"),
            "verification_token must not appear in Debug output"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn feishu_debug_shows_none_for_absent_secrets() {
        let config = FeishuIntakeConfig::default();
        let debug_output = format!("{config:?}");
        assert!(
            debug_output.contains("None"),
            "absent secrets should show as None"
        );
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
