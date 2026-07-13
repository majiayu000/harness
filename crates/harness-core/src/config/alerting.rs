//! Alerting configuration (GH-1582).
//!
//! Inert by default (B-001): with no `[alerting]` section, or with
//! `enabled = false`, nothing is spawned and no behavior changes.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::alert::AlertClass;

fn default_dedup_cooldown_secs() -> u64 {
    300
}

fn default_queue_capacity() -> usize {
    256
}

fn default_shutdown_flush_secs() -> u64 {
    5
}

fn default_max_attempts() -> u32 {
    3
}

fn default_backoff_base_ms() -> u64 {
    500
}

fn default_heartbeat_interval_secs() -> u64 {
    60
}

/// Delivery channel kind. Slack and Feishu are formatters over the same
/// payload; `webhook` posts the raw JSON contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertChannelKind {
    Webhook,
    Slack,
    Feishu,
}

/// One outbound delivery channel (`[[alerting.channels]]`).
#[derive(Clone, Serialize, Deserialize)]
pub struct AlertChannelConfig {
    /// Unique channel name; audit records refer to this name only (B-015).
    pub name: String,
    pub kind: AlertChannelKind,
    /// Destination URL for `webhook`/`slack` kinds. Falls back to the
    /// `HARNESS_ALERT_<NAME>_URL` env var (name uppercased, `-` -> `_`).
    #[serde(default)]
    pub url: Option<String>,
    /// Feishu receive chat id (`feishu` kind only). Credentials come from
    /// the intake Feishu config / `FEISHU_APP_ID` + `FEISHU_APP_SECRET`.
    #[serde(default)]
    pub receive_id: Option<String>,
    /// Delivery attempts per alert before the delivery is marked failed.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Base for exponential backoff between attempts.
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
}

impl AlertChannelConfig {
    fn url_env_var(&self) -> String {
        let name: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        format!("HARNESS_ALERT_{name}_URL")
    }

    /// Configured URL, falling back to the per-channel env var.
    pub fn effective_url(&self) -> Option<String> {
        self.url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .or_else(|| std::env::var(self.url_env_var()).ok())
            .filter(|u| !u.trim().is_empty())
    }
}

impl fmt::Debug for AlertChannelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertChannelConfig")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("url", &self.url.as_ref().map(|_| "[REDACTED]"))
            .field("receive_id", &self.receive_id)
            .field("max_attempts", &self.max_attempts)
            .field("backoff_base_ms", &self.backoff_base_ms)
            .finish()
    }
}

/// Dead-man-switch heartbeat (`[alerting.heartbeat]`, B-016).
#[derive(Clone, Serialize, Deserialize)]
pub struct AlertingHeartbeatConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default = "default_heartbeat_interval_secs")]
    pub interval_secs: u64,
}

impl Default for AlertingHeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            interval_secs: default_heartbeat_interval_secs(),
        }
    }
}

impl fmt::Debug for AlertingHeartbeatConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlertingHeartbeatConfig")
            .field("enabled", &self.enabled)
            .field("url", &self.url.as_ref().map(|_| "[REDACTED]"))
            .field("interval_secs", &self.interval_secs)
            .finish()
    }
}

/// Top-level `[alerting]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertingConfig {
    /// Master switch. Default false: the subsystem is inert (B-001).
    #[serde(default)]
    pub enabled: bool,
    /// Alert classes that fire. Empty = no classes (explicit opt-in, B-003).
    #[serde(default)]
    pub event_classes: Vec<AlertClass>,
    /// Suppression window for alerts sharing a dedup key (B-010).
    #[serde(default = "default_dedup_cooldown_secs")]
    pub dedup_cooldown_secs: u64,
    /// Bounded dispatcher queue capacity (B-011).
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    /// Grace window to flush queued alerts on graceful shutdown (B-018).
    #[serde(default = "default_shutdown_flush_secs")]
    pub shutdown_flush_secs: u64,
    #[serde(default)]
    pub channels: Vec<AlertChannelConfig>,
    #[serde(default)]
    pub heartbeat: AlertingHeartbeatConfig,
}

impl Default for AlertingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            event_classes: Vec::new(),
            dedup_cooldown_secs: default_dedup_cooldown_secs(),
            queue_capacity: default_queue_capacity(),
            shutdown_flush_secs: default_shutdown_flush_secs(),
            channels: Vec::new(),
            heartbeat: AlertingHeartbeatConfig::default(),
        }
    }
}

impl AlertingConfig {
    /// Validate at startup (B-002). Fail closed: an enabled config with an
    /// invalid channel definition is a load error, never a silent skip.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.channels.is_empty() {
            anyhow::bail!("alerting.enabled = true requires at least one [[alerting.channels]]");
        }
        if self.queue_capacity == 0 {
            anyhow::bail!("alerting.queue_capacity must be >= 1");
        }
        let mut names = HashSet::new();
        for channel in &self.channels {
            if channel.name.trim().is_empty() {
                anyhow::bail!("alerting channel with empty name");
            }
            if !names.insert(channel.name.as_str()) {
                anyhow::bail!("duplicate alerting channel name: {}", channel.name);
            }
            if channel.max_attempts == 0 {
                anyhow::bail!(
                    "alerting channel {}: max_attempts must be >= 1",
                    channel.name
                );
            }
            match channel.kind {
                AlertChannelKind::Webhook | AlertChannelKind::Slack => {
                    if channel.effective_url().is_none() {
                        anyhow::bail!(
                            "alerting channel {}: url missing (set url or {})",
                            channel.name,
                            channel.url_env_var()
                        );
                    }
                }
                AlertChannelKind::Feishu => {
                    if channel
                        .receive_id
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .is_empty()
                    {
                        anyhow::bail!(
                            "alerting channel {}: feishu kind requires receive_id",
                            channel.name
                        );
                    }
                }
            }
        }
        if self.heartbeat.enabled {
            if self
                .heartbeat
                .url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                anyhow::bail!("alerting.heartbeat.enabled = true requires heartbeat.url");
            }
            if self.heartbeat.interval_secs == 0 {
                anyhow::bail!("alerting.heartbeat.interval_secs must be >= 1");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_inert_default() {
        let config: AlertingConfig = toml::from_str("").expect("empty section parses");
        assert!(!config.enabled);
        assert!(config.event_classes.is_empty());
        assert!(config.channels.is_empty());
        assert!(!config.heartbeat.enabled);
        config.validate().expect("inert config validates");
    }

    #[test]
    fn disabled_config_skips_channel_validation() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = false
            [[channels]]
            name = ""
            kind = "webhook"
            "#,
        )
        .expect("parse");
        config.validate().expect("disabled config is not validated");
    }

    #[test]
    fn enabled_without_channels_fails() {
        let config: AlertingConfig = toml::from_str("enabled = true").expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn enabled_webhook_without_url_fails() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "ops"
            kind = "webhook"
            "#,
        )
        .expect("parse");
        let err = config.validate().expect_err("missing url must fail");
        assert!(err.to_string().contains("HARNESS_ALERT_OPS_URL"));
    }

    #[test]
    fn enabled_valid_webhook_channel_passes() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            event_classes = ["task_failure_exhausted", "workflow_blocked"]
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://example.invalid/hook"
            "#,
        )
        .expect("parse");
        config.validate().expect("valid config passes");
        assert_eq!(config.event_classes.len(), 2);
        assert_eq!(config.channels[0].max_attempts, 3);
        assert_eq!(config.channels[0].backoff_base_ms, 500);
    }

    #[test]
    fn unknown_event_class_fails_parse() {
        let result = toml::from_str::<AlertingConfig>(
            r#"
            enabled = true
            event_classes = ["not_a_real_class"]
            "#,
        );
        assert!(result.is_err(), "unknown class must fail deserialization");
    }

    #[test]
    fn duplicate_channel_names_fail() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://example.invalid/a"
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://example.invalid/b"
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn feishu_channel_requires_receive_id() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "feishu-ops"
            kind = "feishu"
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_max_attempts_fails() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://example.invalid/hook"
            max_attempts = 0
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn heartbeat_enabled_requires_url() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://example.invalid/hook"
            [heartbeat]
            enabled = true
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn debug_redacts_urls() {
        let config: AlertingConfig = toml::from_str(
            r#"
            enabled = true
            [[channels]]
            name = "ops"
            kind = "webhook"
            url = "https://hooks.example.invalid/secret-token"
            [heartbeat]
            enabled = true
            url = "https://ping.example.invalid/secret-ping"
            "#,
        )
        .expect("parse");
        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("secret-token"));
        assert!(!debug_output.contains("secret-ping"));
    }
}
