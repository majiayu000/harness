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

impl ServerConfig {
    /// Apply environment variable overrides to this config.
    ///
    /// Reads a fixed set of env vars and overwrites the corresponding fields.
    /// Precedence: TOML file → env vars → CLI flags (CLI flags are applied after
    /// this call in the serve command and always win).
    ///
    /// Supported variables:
    /// - `HARNESS_HTTP_ADDR`       — `http_addr` (parsed as `SocketAddr`)
    /// - `HARNESS_DATA_DIR`        — `data_dir`
    /// - `HARNESS_PROJECT_ROOT`    — `project_root`
    /// - `HARNESS_API_TOKEN`       — `api_token`
    /// - `GITHUB_TOKEN`            — `github_token`
    /// - `GITHUB_WEBHOOK_SECRET`   — `github_webhook_secret`
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        if let Ok(v) = std::env::var("HARNESS_DATA_DIR") {
            if !v.is_empty() {
                self.data_dir = std::path::PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("HARNESS_PROJECT_ROOT") {
            if !v.is_empty() {
                self.project_root = std::path::PathBuf::from(v);
            }
        }
        if let Ok(v) = std::env::var("HARNESS_API_TOKEN") {
            if !v.is_empty() {
                self.api_token = Some(v);
            }
        }
        if let Ok(v) = std::env::var("GITHUB_TOKEN") {
            if !v.is_empty() {
                self.github_token = Some(v);
            }
        }
        if let Ok(v) = std::env::var("GITHUB_WEBHOOK_SECRET") {
            if !v.is_empty() {
                self.github_webhook_secret = Some(v);
            }
        }
        Ok(())
    }

    /// Apply the `HARNESS_HTTP_ADDR` env var override.
    ///
    /// This is intentionally separated from [`apply_env_overrides`] because
    /// `HARNESS_HTTP_ADDR` is only meaningful for the `serve` subcommand.
    /// Parsing it eagerly for every subcommand (e.g. `harness exec`,
    /// `harness version`) would make a server-side misconfiguration (blank
    /// value, or a hostname like `localhost:9800` that `SocketAddr` rejects)
    /// brick completely unrelated commands.
    pub fn apply_serve_env_overrides(&mut self) -> anyhow::Result<()> {
        if let Ok(v) = std::env::var("HARNESS_HTTP_ADDR") {
            if !v.is_empty() {
                self.http_addr = v.parse().map_err(|e| {
                    anyhow::anyhow!("HARNESS_HTTP_ADDR={v:?} is not a valid SocketAddr: {e}")
                })?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Exhaustive destructure: compiler error if a new field is added but omitted here.
        let Self {
            transport,
            http_addr,
            data_dir,
            project_root,
            github_webhook_secret,
            notification_broadcast_capacity,
            notification_lag_log_every,
            ws_heartbeat_interval_secs,
            trusted_proxies,
            api_token,
            allowed_project_roots,
            max_webhook_body_bytes,
            signal_rate_limit_per_minute,
            password_reset_rate_limit_per_hour,
            constitution_enabled,
            github_token,
        } = self;
        f.debug_struct("ServerConfig")
            .field("transport", transport)
            .field("http_addr", http_addr)
            .field("data_dir", data_dir)
            .field("project_root", project_root)
            .field(
                "github_webhook_secret",
                &github_webhook_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "notification_broadcast_capacity",
                notification_broadcast_capacity,
            )
            .field("notification_lag_log_every", notification_lag_log_every)
            .field("ws_heartbeat_interval_secs", ws_heartbeat_interval_secs)
            .field("trusted_proxies", trusted_proxies)
            .field("api_token", &api_token.as_ref().map(|_| "[REDACTED]"))
            .field("allowed_project_roots", allowed_project_roots)
            .field("max_webhook_body_bytes", max_webhook_body_bytes)
            .field("signal_rate_limit_per_minute", signal_rate_limit_per_minute)
            .field(
                "password_reset_rate_limit_per_hour",
                password_reset_rate_limit_per_hour,
            )
            .field("constitution_enabled", constitution_enabled)
            .field("github_token", &github_token.as_ref().map(|_| "[REDACTED]"))
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

    // --- env override tests ---

    #[test]
    fn env_override_http_addr() {
        temp_env::with_vars([("HARNESS_HTTP_ADDR", Some("127.0.0.1:9801"))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_serve_env_overrides().unwrap();
            assert_eq!(cfg.http_addr.port(), 9801);
        });
    }

    #[test]
    fn env_override_http_addr_empty_leaves_default() {
        temp_env::with_vars([("HARNESS_HTTP_ADDR", Some(""))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_serve_env_overrides().unwrap();
            assert_eq!(cfg.http_addr.port(), 9800);
        });
    }

    #[test]
    fn env_override_data_dir() {
        temp_env::with_vars([("HARNESS_DATA_DIR", Some("/tmp/testdir"))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.data_dir, std::path::PathBuf::from("/tmp/testdir"));
        });
    }

    #[test]
    fn env_override_empty_data_dir_does_not_override() {
        temp_env::with_vars([("HARNESS_DATA_DIR", Some(""))], || {
            let default = ServerConfig::default();
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.data_dir, default.data_dir);
        });
    }

    #[test]
    fn env_override_empty_project_root_does_not_override() {
        temp_env::with_vars([("HARNESS_PROJECT_ROOT", Some(""))], || {
            let default = ServerConfig::default();
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.project_root, default.project_root);
        });
    }

    #[test]
    fn env_override_api_token() {
        temp_env::with_vars([("HARNESS_API_TOKEN", Some("tok-test"))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.api_token, Some("tok-test".to_string()));
        });
    }

    #[test]
    fn env_override_github_token_fallback() {
        temp_env::with_vars([("GITHUB_TOKEN", Some("gh-test"))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.github_token, Some("gh-test".to_string()));
        });
    }

    #[test]
    fn env_override_invalid_addr_returns_error() {
        temp_env::with_vars([("HARNESS_HTTP_ADDR", Some("not-an-addr"))], || {
            let mut cfg = ServerConfig::default();
            let result = cfg.apply_serve_env_overrides();
            assert!(result.is_err());
        });
    }

    #[test]
    fn env_override_empty_api_token_does_not_override_toml_token() {
        temp_env::with_vars([("HARNESS_API_TOKEN", Some(""))], || {
            let mut cfg = ServerConfig {
                api_token: Some("real-token".to_string()),
                ..ServerConfig::default()
            };
            cfg.apply_env_overrides().unwrap();
            // Empty env var must not overwrite a valid TOML token.
            assert_eq!(cfg.api_token, Some("real-token".to_string()));
        });
    }

    #[test]
    fn env_override_empty_github_token_does_not_override_toml_token() {
        temp_env::with_vars([("GITHUB_TOKEN", Some(""))], || {
            let mut cfg = ServerConfig {
                github_token: Some("gh-real".to_string()),
                ..ServerConfig::default()
            };
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.github_token, Some("gh-real".to_string()));
        });
    }

    #[test]
    fn env_override_github_webhook_secret() {
        temp_env::with_vars([("GITHUB_WEBHOOK_SECRET", Some("wh-secret-env"))], || {
            let mut cfg = ServerConfig::default();
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.github_webhook_secret, Some("wh-secret-env".to_string()));
        });
    }

    #[test]
    fn env_override_empty_github_webhook_secret_does_not_override_toml_secret() {
        temp_env::with_vars([("GITHUB_WEBHOOK_SECRET", Some(""))], || {
            let mut cfg = ServerConfig {
                github_webhook_secret: Some("wh-real".to_string()),
                ..ServerConfig::default()
            };
            cfg.apply_env_overrides().unwrap();
            assert_eq!(cfg.github_webhook_secret, Some("wh-real".to_string()));
        });
    }

    #[test]
    fn env_override_absent_vars_leave_defaults() {
        temp_env::with_vars(
            [
                ("HARNESS_HTTP_ADDR", None::<&str>),
                ("HARNESS_DATA_DIR", None::<&str>),
                ("HARNESS_PROJECT_ROOT", None::<&str>),
                ("HARNESS_API_TOKEN", None::<&str>),
                ("GITHUB_TOKEN", None::<&str>),
            ],
            || {
                let mut cfg = ServerConfig::default();
                // Snapshot before calling apply_env_overrides to avoid a race
                // with parallel tests that temporarily mutate HOME.
                let http_addr_before = cfg.http_addr;
                let data_dir_before = cfg.data_dir.clone();
                cfg.apply_env_overrides().unwrap();
                assert_eq!(cfg.http_addr, http_addr_before);
                assert_eq!(cfg.data_dir, data_dir_before);
                assert_eq!(cfg.api_token, None);
                assert_eq!(cfg.github_token, None);
            },
        );
    }

    // --- existing tests ---

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
            debug_output.contains("github_webhook_secret: None"),
            "absent github_webhook_secret should show as None"
        );
        assert!(
            debug_output.contains("api_token: None"),
            "absent api_token should show as None"
        );
        assert!(
            debug_output.contains("github_token: None"),
            "absent github_token should show as None"
        );
    }
}
