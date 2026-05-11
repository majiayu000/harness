use serde::{Deserialize, Serialize};

/// HTTP server shutdown configuration.
///
/// Controls the staged drain-then-force shutdown state machine. The HTTP
/// listener stops accepting new connections on the first signal, then waits
/// up to `drain_timeout_secs` for in-flight requests to finish. A second
/// signal (or the timeout) forces graceful shutdown to end. A hard outer
/// deadline of `drain_timeout_secs + force_grace_secs` guarantees the
/// process eventually exits even if the runtime itself is wedged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownConfig {
    /// Seconds to wait for in-flight HTTP requests to drain naturally
    /// before forcing the graceful shutdown to end. Default: 30.
    #[serde(default = "default_shutdown_drain_secs")]
    pub drain_timeout_secs: u64,
    /// Additional seconds beyond `drain_timeout_secs` before the process
    /// hard-exits. The hard deadline is `drain_timeout_secs + force_grace_secs`.
    /// Default: 30.
    #[serde(default = "default_shutdown_force_grace_secs")]
    pub force_grace_secs: u64,
    /// Seconds between drain progress logs while waiting for in-flight
    /// requests to complete. Default: 5.
    #[serde(default = "default_shutdown_progress_log_secs")]
    pub progress_log_secs: u64,
}

fn default_shutdown_drain_secs() -> u64 {
    30
}

fn default_shutdown_force_grace_secs() -> u64 {
    30
}

fn default_shutdown_progress_log_secs() -> u64 {
    5
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            drain_timeout_secs: default_shutdown_drain_secs(),
            force_grace_secs: default_shutdown_force_grace_secs(),
            progress_log_secs: default_shutdown_progress_log_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_values() {
        let cfg = ShutdownConfig::default();
        assert_eq!(cfg.drain_timeout_secs, 30);
        assert_eq!(cfg.force_grace_secs, 30);
        assert_eq!(cfg.progress_log_secs, 5);
    }

    #[test]
    fn deserializes_from_toml() {
        let toml_str = r#"
            drain_timeout_secs = 60
            force_grace_secs = 15
            progress_log_secs = 2
        "#;
        let cfg: ShutdownConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.drain_timeout_secs, 60);
        assert_eq!(cfg.force_grace_secs, 15);
        assert_eq!(cfg.progress_log_secs, 2);
    }

    #[test]
    fn deserializes_with_partial_fields() {
        let toml_str = r#"
            drain_timeout_secs = 90
        "#;
        let cfg: ShutdownConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.drain_timeout_secs, 90);
        assert_eq!(cfg.force_grace_secs, 30);
        assert_eq!(cfg.progress_log_secs, 5);
    }
}
