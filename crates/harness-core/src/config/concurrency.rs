use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Concurrency limiting configuration for task execution.
///
/// Controls how many tasks run simultaneously and how many can wait
/// in the queue before new submissions are rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyConfig {
    /// Maximum number of tasks executing concurrently across all projects. Default: 4.
    #[serde(default = "default_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
    /// Maximum number of tasks waiting for a slot. Excess tasks are rejected. Default: 32.
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    /// Seconds of silence from the agent stream before declaring a stall. Default: 600.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,
    /// Per-project concurrency limits.
    ///
    /// Maps **canonical filesystem path** → max concurrent tasks for that project.
    /// Keys must be the absolute, canonical path to the project root — the same
    /// value produced by `ProjectId::from_path(&root)` — because the queue and
    /// registry both key on that form. Non-absolute keys are accepted but will
    /// never match any project at runtime (a warning is emitted on startup).
    ///
    /// Projects not listed here use the global `max_concurrent_tasks` limit.
    ///
    /// Example:
    /// ```toml
    /// [concurrency.per_project]
    /// "/home/user/my-project" = 2
    /// ```
    #[serde(default)]
    pub per_project: HashMap<String, usize>,
    /// Maximum total agent API calls across all phases (implementation + validation retries +
    /// review rounds). `None` = unlimited. Counts every call including validation retries.
    /// Recommended production value: 20.
    #[serde(default = "default_max_turns")]
    pub max_turns: Option<u32>,
    /// Jaccard word-similarity threshold for review-loop detection.
    /// If two consecutive non-waiting review outputs have similarity >= this value,
    /// the task is marked Failed with "review loop detected". Default: 0.85.
    #[serde(default = "default_loop_jaccard_threshold")]
    pub loop_jaccard_threshold: f64,
    /// Minimum available system memory (MB) required to admit new tasks.
    /// When available memory falls below this threshold the task queue
    /// rejects new `acquire()` calls until memory recovers.
    /// `None` (default) disables the check entirely.
    #[serde(default)]
    pub memory_pressure_threshold_mb: Option<u64>,
    /// How often (seconds) the memory monitor re-samples available memory.
    /// Values below 1 are clamped to 1. Default: 5.
    /// Only meaningful when `memory_pressure_threshold_mb` is `Some`.
    #[serde(default = "default_memory_poll_interval_secs")]
    pub memory_poll_interval_secs: u64,
    /// Priority aging (anti-starvation) for the task queue.
    ///
    /// Example:
    /// ```toml
    /// [concurrency.aging]
    /// enabled = true
    /// interval_secs = 300
    /// max_boost_levels = 2
    /// ```
    #[serde(default)]
    pub aging: PriorityAgingConfig,
}

fn default_max_concurrent_tasks() -> usize {
    4
}

fn default_max_queue_size() -> usize {
    32
}

fn default_stall_timeout_secs() -> u64 {
    super::stall_timeout::DEFAULT_STALL_TIMEOUT_SECS
}

fn default_loop_jaccard_threshold() -> f64 {
    0.85
}

fn default_memory_poll_interval_secs() -> u64 {
    5
}

fn default_max_turns() -> Option<u32> {
    Some(20)
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: default_max_concurrent_tasks(),
            max_queue_size: default_max_queue_size(),
            stall_timeout_secs: default_stall_timeout_secs(),
            per_project: HashMap::new(),
            max_turns: default_max_turns(),
            loop_jaccard_threshold: default_loop_jaccard_threshold(),
            memory_pressure_threshold_mb: None,
            memory_poll_interval_secs: default_memory_poll_interval_secs(),
            aging: PriorityAgingConfig::default(),
        }
    }
}

/// Priority aging configuration for the task queue (anti-starvation).
///
/// While a task waits for an execution slot, its effective priority is boosted
/// by one level per `interval_secs` of wait, capped at
/// `base + max_boost_levels` (and never above the maximum priority level).
/// Setting `enabled = false` restores strict `(priority DESC, FIFO within
/// level)` ordering exactly.
#[derive(Debug, Clone, Serialize)]
pub struct PriorityAgingConfig {
    /// Whether priority aging is enabled. Default: true.
    pub enabled: bool,
    /// Seconds of wait per one effective-priority boost level. Default: 300.
    /// Must be non-zero when `enabled` is true (rejected at config load).
    pub interval_secs: u64,
    /// Maximum boost above the base priority. Default: 2.
    pub max_boost_levels: u8,
}

fn default_aging_enabled() -> bool {
    true
}

fn default_aging_interval_secs() -> u64 {
    300
}

fn default_aging_max_boost_levels() -> u8 {
    2
}

impl Default for PriorityAgingConfig {
    fn default() -> Self {
        Self {
            enabled: default_aging_enabled(),
            interval_secs: default_aging_interval_secs(),
            max_boost_levels: default_aging_max_boost_levels(),
        }
    }
}

impl<'de> Deserialize<'de> for PriorityAgingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct PriorityAgingConfigToml {
            #[serde(default = "default_aging_enabled")]
            enabled: bool,
            #[serde(default = "default_aging_interval_secs")]
            interval_secs: u64,
            #[serde(default = "default_aging_max_boost_levels")]
            max_boost_levels: u8,
        }

        let toml = PriorityAgingConfigToml::deserialize(deserializer)?;
        if toml.enabled && toml.interval_secs == 0 {
            return Err(serde::de::Error::custom(
                "[concurrency.aging] interval_secs must be non-zero when aging is enabled \
                 (set enabled = false to disable aging instead)",
            ));
        }
        Ok(Self {
            enabled: toml.enabled,
            interval_secs: toml.interval_secs,
            max_boost_levels: toml.max_boost_levels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_disables_monitor() {
        let cfg = ConcurrencyConfig::default();
        assert!(
            cfg.memory_pressure_threshold_mb.is_none(),
            "memory monitor must be disabled by default"
        );
        assert_eq!(cfg.max_turns, Some(20));
        assert_eq!(cfg.memory_poll_interval_secs, 5);
    }

    #[test]
    fn toml_roundtrip_with_threshold() {
        let toml = r#"
            memory_pressure_threshold_mb = 512
            memory_poll_interval_secs = 10
        "#;
        let cfg: ConcurrencyConfig = toml::from_str(toml).expect("toml parse failed");
        assert_eq!(cfg.memory_pressure_threshold_mb, Some(512));
        assert_eq!(cfg.memory_poll_interval_secs, 10);
    }

    #[test]
    fn toml_roundtrip_without_threshold_uses_defaults() {
        let cfg: ConcurrencyConfig = toml::from_str("").expect("toml parse failed");
        assert!(cfg.memory_pressure_threshold_mb.is_none());
        assert_eq!(cfg.memory_poll_interval_secs, 5);
    }

    #[test]
    fn aging_defaults_are_on_with_conservative_slope() {
        // B-009: unconfigured aging deserializes to enabled=true, 300s, +2.
        let cfg: ConcurrencyConfig = toml::from_str("").expect("toml parse failed");
        assert!(cfg.aging.enabled, "aging must be ON by default");
        assert_eq!(cfg.aging.interval_secs, 300);
        assert_eq!(cfg.aging.max_boost_levels, 2);
        // Default::default() must agree with the serde defaults.
        let def = ConcurrencyConfig::default();
        assert!(def.aging.enabled);
        assert_eq!(def.aging.interval_secs, 300);
        assert_eq!(def.aging.max_boost_levels, 2);
    }

    #[test]
    fn aging_section_overrides_apply() {
        let toml = r#"
            [aging]
            enabled = false
            interval_secs = 60
            max_boost_levels = 1
        "#;
        let cfg: ConcurrencyConfig = toml::from_str(toml).expect("toml parse failed");
        assert!(!cfg.aging.enabled);
        assert_eq!(cfg.aging.interval_secs, 60);
        assert_eq!(cfg.aging.max_boost_levels, 1);
    }

    #[test]
    fn aging_enabled_with_zero_interval_is_rejected_at_load() {
        // B-010: invalid aging config errors at load, no silent clamp.
        let toml = r#"
            [aging]
            enabled = true
            interval_secs = 0
        "#;
        let err = toml::from_str::<ConcurrencyConfig>(toml)
            .expect_err("interval_secs = 0 with aging enabled must be rejected");
        assert!(
            err.to_string().contains("interval_secs must be non-zero"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn aging_disabled_with_zero_interval_is_accepted() {
        // Disabled aging never divides by the interval; zero is tolerated.
        let toml = r#"
            [aging]
            enabled = false
            interval_secs = 0
        "#;
        let cfg: ConcurrencyConfig = toml::from_str(toml).expect("toml parse failed");
        assert!(!cfg.aging.enabled);
        assert_eq!(cfg.aging.interval_secs, 0);
    }
}
