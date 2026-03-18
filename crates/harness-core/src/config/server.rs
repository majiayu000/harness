use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

use super::dirs::dirs_data_dir;

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
