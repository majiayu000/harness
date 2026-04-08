use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use super::dirs::dirs_data_dir;

#[derive(Clone, Serialize, Deserialize)]
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
    /// Interval in seconds between WebSocket heartbeat pings. Must be >= 1.
    #[serde(default = "default_ws_heartbeat_interval_secs")]
    pub ws_heartbeat_interval_secs: u64,
    /// List of trusted proxy IP addresses. X-Forwarded-For is only used when the
    /// request originates from one of these IPs.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Static bearer token for API authentication.
    ///
    /// When set (via this field or the `HARNESS_API_TOKEN` environment variable),
    /// all non-webhook and non-health HTTP endpoints require an
    /// `Authorization: Bearer <token>` header. When not configured, authentication
    /// is skipped for backward-compatible local development.
    #[serde(default)]
    pub api_token: Option<String>,
    /// Allowlist of base directories under which project roots may be registered.
    ///
    /// When non-empty, `POST /projects` rejects any root that is not a descendant
    /// of one of these directories. When empty, any valid git repository path is
    /// accepted (legacy behaviour).
    #[serde(default)]
    pub allowed_project_roots: Vec<PathBuf>,
    /// Maximum allowed body size in bytes for webhook and signal ingestion endpoints.
    /// Default: 524288 (512 KiB).
    #[serde(default = "default_max_webhook_body_bytes")]
    pub max_webhook_body_bytes: usize,
    /// Maximum number of signal ingestion requests per source per minute.
    /// Default: 100.
    #[serde(default = "default_signal_rate_limit_per_minute")]
    pub signal_rate_limit_per_minute: u32,
    /// Maximum number of password reset requests per identifier (email) per hour.
    /// Default: 5.
    #[serde(default = "default_password_reset_rate_limit_per_hour")]
    pub password_reset_rate_limit_per_hour: u32,
    /// Prepend the Golden Principles constitution to every agent prompt.
    /// Default: true.
    #[serde(default = "default_constitution_enabled")]
    pub constitution_enabled: bool,
    /// GitHub personal access token for review bot auto-trigger.
    ///
    /// Used to post comments on PRs (e.g. `@gemini-code-assist review`).
    /// Falls back to `GITHUB_TOKEN` env var when not configured.
    #[serde(default)]
    pub github_token: Option<String>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("transport", &self.transport)
            .field("http_addr", &self.http_addr)
            .field("data_dir", &self.data_dir)
            .field("project_root", &self.project_root)
            .field(
                "github_webhook_secret",
                &self.github_webhook_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "notification_broadcast_capacity",
                &self.notification_broadcast_capacity,
            )
            .field(
                "notification_lag_log_every",
                &self.notification_lag_log_every,
            )
            .field(
                "ws_heartbeat_interval_secs",
                &self.ws_heartbeat_interval_secs,
            )
            .field("trusted_proxies", &self.trusted_proxies)
            .field("api_token", &self.api_token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_project_roots", &self.allowed_project_roots)
            .field("max_webhook_body_bytes", &self.max_webhook_body_bytes)
            .field(
                "signal_rate_limit_per_minute",
                &self.signal_rate_limit_per_minute,
            )
            .field(
                "password_reset_rate_limit_per_hour",
                &self.password_reset_rate_limit_per_hour,
            )
            .field("constitution_enabled", &self.constitution_enabled)
            .field(
                "github_token",
                &self.github_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Http,
            http_addr: SocketAddr::from(([127, 0, 0, 1], 9800)),
            data_dir: dirs_data_dir().join("harness"),
            project_root: default_project_root(),
            github_webhook_secret: None,
            notification_broadcast_capacity: default_notification_broadcast_capacity(),
            notification_lag_log_every: default_notification_lag_log_every(),
            ws_heartbeat_interval_secs: default_ws_heartbeat_interval_secs(),
            trusted_proxies: Vec::new(),
            api_token: None,
            allowed_project_roots: Vec::new(),
            max_webhook_body_bytes: default_max_webhook_body_bytes(),
            signal_rate_limit_per_minute: default_signal_rate_limit_per_minute(),
            password_reset_rate_limit_per_hour: default_password_reset_rate_limit_per_hour(),
            constitution_enabled: true,
            github_token: None,
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

fn default_project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn default_notification_broadcast_capacity() -> usize {
    256
}

fn default_notification_lag_log_every() -> u64 {
    1
}

fn default_ws_heartbeat_interval_secs() -> u64 {
    30
}

fn default_max_webhook_body_bytes() -> usize {
    524288
}

fn default_signal_rate_limit_per_minute() -> u32 {
    100
}

fn default_password_reset_rate_limit_per_hour() -> u32 {
    5
}

fn default_constitution_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_debug_redacts_secrets() {
        let config = ServerConfig {
            github_webhook_secret: Some("wh-secret-abc".to_string()),
            api_token: Some("tok-xyz".to_string()),
            github_token: Some("gh-token-123".to_string()),
            ..ServerConfig::default()
        };
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("wh-secret-abc"),
            "github_webhook_secret must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("tok-xyz"),
            "api_token must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("gh-token-123"),
            "github_token must not appear in Debug output"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn server_config_debug_shows_none_for_absent_secrets() {
        let config = ServerConfig::default();
        let debug_output = format!("{config:?}");
        assert!(
            debug_output.contains("None"),
            "absent secrets should show as None"
        );
    }
}
