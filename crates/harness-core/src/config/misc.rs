use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::dirs::dirs_data_dir;

/// Workspace isolation configuration for parallel task execution.
///
/// WorkspaceManager provisions an isolated git worktree per task,
/// preventing merge conflicts when multiple agents edit the same project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Root directory for all task worktrees. Default: `~/.local/share/harness/workspaces`.
    #[serde(default = "default_workspace_root")]
    pub root: PathBuf,
    /// Shell script run after worktree creation (cwd = workspace). Fatal on failure.
    #[serde(default)]
    pub after_create_hook: Option<String>,
    /// Shell script run before worktree removal (cwd = workspace). Non-fatal on failure.
    #[serde(default)]
    pub before_remove_hook: Option<String>,
    /// Timeout in seconds for hook execution. Default: 60.
    #[serde(default = "default_hook_timeout_secs")]
    pub hook_timeout_secs: u64,
    /// If true, remove workspace when task reaches Done or Failed state. Default: true.
    #[serde(default = "default_auto_cleanup")]
    pub auto_cleanup: bool,
}

fn default_workspace_root() -> PathBuf {
    dirs_data_dir().join("harness").join("workspaces")
}

fn default_hook_timeout_secs() -> u64 {
    60
}

fn default_auto_cleanup() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: default_workspace_root(),
            after_create_hook: None,
            before_remove_hook: None,
            hook_timeout_secs: default_hook_timeout_secs(),
            auto_cleanup: default_auto_cleanup(),
        }
    }
}

/// Concurrency limiting configuration for task execution.
///
/// Controls how many tasks run simultaneously and how many can wait
/// in the queue before new submissions are rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyConfig {
    /// Maximum number of tasks executing concurrently. Default: 4.
    #[serde(default = "default_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
    /// Maximum number of tasks waiting for a slot. Excess tasks are rejected. Default: 32.
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
}

fn default_max_concurrent_tasks() -> usize {
    4
}

fn default_max_queue_size() -> usize {
    32
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: default_max_concurrent_tasks(),
            max_queue_size: default_max_queue_size(),
        }
    }
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
