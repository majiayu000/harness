use crate::types::Grade;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::dirs::dirs_data_dir;

/// Workspace isolation configuration for parallel task execution.
///
/// WorkspaceManager provisions an isolated git worktree per task,
/// preventing merge conflicts when multiple agents edit the same project.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceConfig {
    /// Root directory for all task worktrees. Runtime default: `[server.data_dir]/workspaces`.
    #[serde(default = "default_workspace_root")]
    pub root: PathBuf,
    /// Tracks whether `root` came from config so derived defaults do not rewrite
    /// an explicit legacy root value.
    #[doc(hidden)]
    #[serde(skip)]
    pub root_configured: bool,
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

pub fn default_workspace_root() -> PathBuf {
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
            root_configured: false,
            after_create_hook: None,
            before_remove_hook: None,
            hook_timeout_secs: default_hook_timeout_secs(),
            auto_cleanup: default_auto_cleanup(),
        }
    }
}

impl<'de> Deserialize<'de> for WorkspaceConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WorkspaceConfigToml {
            #[serde(default)]
            root: Option<PathBuf>,
            #[serde(default)]
            after_create_hook: Option<String>,
            #[serde(default)]
            before_remove_hook: Option<String>,
            #[serde(default = "default_hook_timeout_secs")]
            hook_timeout_secs: u64,
            #[serde(default = "default_auto_cleanup")]
            auto_cleanup: bool,
        }

        let toml = WorkspaceConfigToml::deserialize(deserializer)?;
        let root_configured = toml.root.is_some();
        Ok(Self {
            root: toml.root.unwrap_or_else(default_workspace_root),
            root_configured,
            after_create_hook: toml.after_create_hook,
            before_remove_hook: toml.before_remove_hook,
            hook_timeout_secs: toml.hook_timeout_secs,
            auto_cleanup: toml.auto_cleanup,
        })
    }
}

impl WorkspaceConfig {
    pub fn root_for_data_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("workspaces")
    }

    pub fn use_data_dir_default_root(&mut self, data_dir: &Path) {
        if !self.root_configured && self.root == default_workspace_root() {
            self.root = Self::root_for_data_dir(data_dir);
        }
    }
}

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
    #[serde(default)]
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

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: default_max_concurrent_tasks(),
            max_queue_size: default_max_queue_size(),
            stall_timeout_secs: default_stall_timeout_secs(),
            per_project: HashMap::new(),
            max_turns: None,
            loop_jaccard_threshold: default_loop_jaccard_threshold(),
            memory_pressure_threshold_mb: None,
            memory_poll_interval_secs: default_memory_poll_interval_secs(),
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
    /// Timeout in seconds for the LGTM test gate command. Default: 300.
    #[serde(default = "default_test_gate_timeout_secs")]
    pub test_gate_timeout_secs: u64,
}

fn default_validation_timeout_secs() -> u64 {
    120
}

fn default_validation_max_retries() -> u32 {
    2
}

fn default_test_gate_timeout_secs() -> u64 {
    300
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            pre_commit: Vec::new(),
            pre_push: Vec::new(),
            timeout_secs: default_validation_timeout_secs(),
            max_retries: default_validation_max_retries(),
            test_gate_timeout_secs: default_test_gate_timeout_secs(),
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
    /// Hours after which a pending draft is considered stale and removed. Default: 72.
    #[serde(default = "default_gc_draft_ttl_hours")]
    pub draft_ttl_hours: u64,
    pub signal_thresholds: SignalThresholdsConfig,
    /// Grades that trigger an automatic GC run after task completion. Default: [D].
    #[serde(default = "default_auto_gc_grades")]
    pub auto_gc_grades: Vec<Grade>,
    /// Minimum seconds between auto-triggered GC runs (cooldown). Default: 300.
    #[serde(default = "default_auto_gc_cooldown_secs")]
    pub auto_gc_cooldown_secs: u64,
    /// When true, `gc adopt` automatically creates a branch, commits the fix, pushes,
    /// and opens a PR via the agent prompt flow. Default: true.
    #[serde(default = "default_gc_auto_pr")]
    pub auto_pr: bool,
    /// Tools allowed during GC agent execution. Default: ["Read", "Grep", "Glob"].
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Auto-adoption policy for drafts produced by `gc_agent.run`.
    /// Default: `Off` — all drafts remain `Pending` and require explicit
    /// `gc adopt` from the CLI or UI. See `AutoAdoptPolicy` for other modes.
    #[serde(default)]
    pub auto_adopt: AutoAdoptPolicy,
    /// Target-path prefix (relative to project root) required for auto-adopt
    /// to apply. Drafts whose artifacts target paths outside this prefix are
    /// left `Pending` even when `auto_adopt` is enabled. Default: `.harness/generated/`.
    #[serde(default = "default_auto_adopt_path_prefix")]
    pub auto_adopt_path_prefix: String,
    /// Maximum seconds to wait for `gc_agent.run` in the task-completion
    /// handler before giving up and logging a warning. Default: 120.
    #[serde(default = "default_gc_run_timeout_secs")]
    pub gc_run_timeout_secs: u64,
}

/// Controls whether drafts produced by a GC run are automatically moved to
/// `Adopted` status (which writes their artifacts into the project tree).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoAdoptPolicy {
    /// No auto-adoption. Drafts stay `Pending` until a human adopts them.
    #[default]
    Off,
    /// Auto-adopt drafts whose signal remediation is `Rule`, provided all
    /// artifact target paths fall under `auto_adopt_path_prefix`.
    RulesOnly,
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
            draft_ttl_hours: default_gc_draft_ttl_hours(),
            signal_thresholds: SignalThresholdsConfig::default(),
            auto_gc_grades: default_auto_gc_grades(),
            auto_gc_cooldown_secs: default_auto_gc_cooldown_secs(),
            auto_pr: default_gc_auto_pr(),
            allowed_tools: None,
            auto_adopt: AutoAdoptPolicy::default(),
            auto_adopt_path_prefix: default_auto_adopt_path_prefix(),
            gc_run_timeout_secs: default_gc_run_timeout_secs(),
        }
    }
}

fn default_auto_adopt_path_prefix() -> String {
    ".harness/generated/".to_string()
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

fn default_gc_draft_ttl_hours() -> u64 {
    72
}

fn default_auto_gc_grades() -> Vec<Grade> {
    // Any grade below A triggers GC. Previously this was `[Grade::D]` alone,
    // which meant an otherwise-uncalibrated grader that rarely (or never)
    // emitted D silently disabled the entire GC feedback loop. Widening the
    // trigger set to B/C/D ensures any degradation produces a draft.
    vec![Grade::B, Grade::C, Grade::D]
}

fn default_auto_gc_cooldown_secs() -> u64 {
    300
}

fn default_gc_run_timeout_secs() -> u64 {
    120
}

fn default_gc_auto_pr() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalThresholdsConfig {
    pub repeated_warn_min: usize,
    pub chronic_block_min: usize,
    pub hot_file_edits_min: usize,
    pub slow_op_threshold_ms: u64,
    pub slow_op_count_min: usize,
    pub escalation_ratio: f64,
    pub violation_min: usize,
}

impl Default for SignalThresholdsConfig {
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
    /// Apply fix_pattern automatically when a violation has one. Default: false (opt-in).
    #[serde(default)]
    pub auto_fix: bool,
    /// Enable real-time hook enforcement: scan modified files after each agent turn
    /// and inject any violations into the next turn prompt. Default: false.
    #[serde(default)]
    pub hook_enforcement: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    Shadow,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextQuotasConfig {
    #[serde(default = "default_context_rule_quota")]
    pub rule: f32,
    #[serde(default = "default_context_skill_quota")]
    pub skill: f32,
    #[serde(default = "default_context_contract_quota")]
    pub contract: f32,
    #[serde(default = "default_context_brief_quota")]
    pub brief: f32,
    #[serde(default = "default_context_draft_quota")]
    pub draft: f32,
}

fn default_context_rule_quota() -> f32 {
    0.30
}

fn default_context_skill_quota() -> f32 {
    0.25
}

fn default_context_contract_quota() -> f32 {
    0.25
}

fn default_context_brief_quota() -> f32 {
    0.15
}

fn default_context_draft_quota() -> f32 {
    0.05
}

impl Default for ContextQuotasConfig {
    fn default() -> Self {
        Self {
            rule: default_context_rule_quota(),
            skill: default_context_skill_quota(),
            contract: default_context_contract_quota(),
            brief: default_context_brief_quota(),
            draft: default_context_draft_quota(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default)]
    pub mode: ContextMode,
    #[serde(default = "default_context_budget_tokens")]
    pub budget_tokens: u32,
    #[serde(default = "default_context_reserved_headroom")]
    pub reserved_headroom: f32,
    #[serde(default = "default_context_provider_timeout_ms")]
    pub provider_timeout_ms: u64,
    #[serde(default)]
    pub quotas: ContextQuotasConfig,
}

fn default_context_budget_tokens() -> u32 {
    24_000
}

fn default_context_reserved_headroom() -> f32 {
    0.20
}

fn default_context_provider_timeout_ms() -> u64 {
    2_000
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            mode: ContextMode::Shadow,
            budget_tokens: default_context_budget_tokens(),
            reserved_headroom: default_context_reserved_headroom(),
            provider_timeout_ms: default_context_provider_timeout_ms(),
            quotas: ContextQuotasConfig::default(),
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
    #[serde(default)]
    pub trajectory: bool,
    #[serde(default)]
    pub capture_content: bool,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            environment: default_otel_environment(),
            exporter: OtelExporter::default(),
            endpoint: None,
            log_user_prompt: false,
            trajectory: false,
            capture_content: false,
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

/// Periodic codebase review configuration.
///
/// When enabled, the scheduler spawns an agent review job at the configured interval.
/// The review is skipped if no new commits have landed since the last review.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewStrategy {
    /// Run one reviewer task per cycle.
    #[default]
    Single,
    /// Run two reviewer tasks and synthesize into a final report.
    Cross,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// Whether periodic review is enabled. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Run a review immediately on server startup (after a brief init delay).
    /// Default: false.
    #[serde(default)]
    pub run_on_startup: bool,
    /// Interval between review runs in hours. Default: 24.
    /// Ignored when `interval_secs` is set.
    #[serde(default = "default_review_interval_hours")]
    pub interval_hours: u64,
    /// Override interval in seconds for fine-grained control (e.g. testing).
    /// Takes precedence over `interval_hours` when set.
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Agent to use for the review. Default: None (use the default agent).
    #[serde(default)]
    pub agent: Option<String>,
    /// Review strategy. `single` runs one reviewer; `cross` runs dual-review + synthesis.
    #[serde(default)]
    pub strategy: ReviewStrategy,
    /// Per-turn timeout in seconds for the review agent. Default: 900.
    #[serde(default = "default_review_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum number of periodic review tasks executing concurrently.
    /// Uses a dedicated review capacity domain and does not consume issue/planner slots.
    #[serde(default = "default_review_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
}

impl ReviewConfig {
    /// Effective interval as a Duration. `interval_secs` takes precedence
    /// over `interval_hours`.
    pub fn effective_interval(&self) -> std::time::Duration {
        match self.interval_secs {
            Some(secs) => std::time::Duration::from_secs(secs),
            None => std::time::Duration::from_secs(self.interval_hours * 3600),
        }
    }
}

fn default_review_interval_hours() -> u64 {
    24
}

fn default_review_timeout_secs() -> u64 {
    900
}

fn default_review_max_concurrent_tasks() -> usize {
    2
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            run_on_startup: false,
            interval_hours: default_review_interval_hours(),
            interval_secs: None,
            agent: None,
            strategy: ReviewStrategy::Single,
            timeout_secs: default_review_timeout_secs(),
            max_concurrent_tasks: default_review_max_concurrent_tasks(),
        }
    }
}

/// Configuration for the periodic retry scheduler.
///
/// The scheduler scans for stalled in-flight tasks (tasks in active statuses
/// whose `updated_at` is older than `stale_threshold_mins`) and re-enqueues
/// them up to `max_retries` times before labelling the issue `harness:stuck`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrySchedulerConfig {
    /// Whether the periodic retry scheduler is enabled. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between scheduler ticks. Default: 300 (5 minutes).
    #[serde(default = "default_retry_interval_secs")]
    pub interval_secs: u64,
    /// A task is considered stalled when `updated_at` is older than this many
    /// minutes. Default: 60.
    #[serde(default = "default_retry_stale_threshold_mins")]
    pub stale_threshold_mins: u64,
    /// Minimum gap in minutes between two retries of the same task. Default: 120.
    #[serde(default = "default_retry_cooldown_mins")]
    pub cooldown_mins: u64,
    /// Maximum number of automatic retries before the task is labelled
    /// `harness:stuck`. Default: 3.
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
}

fn default_retry_interval_secs() -> u64 {
    300
}

fn default_retry_stale_threshold_mins() -> u64 {
    60
}

fn default_retry_cooldown_mins() -> u64 {
    120
}

fn default_retry_max_retries() -> u32 {
    3
}

impl Default for RetrySchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_retry_interval_secs(),
            stale_threshold_mins: default_retry_stale_threshold_mins(),
            cooldown_mins: default_retry_cooldown_mins(),
            max_retries: default_retry_max_retries(),
        }
    }
}

/// Configuration for the periodic GitHub ↔ Harness reconciliation loop.
///
/// The loop enumerates every non-terminal task that has a `pr_url` or an
/// `external_id` like `issue:N` / `pr:N`, fetches the current GitHub state
/// via the GitHub API, and applies the appropriate task-status transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationConfig {
    /// Whether the reconciliation loop is enabled. Default: true.
    #[serde(default = "default_reconciliation_enabled")]
    pub enabled: bool,
    /// Seconds between reconciliation ticks. Default: 300 (5 minutes).
    #[serde(default = "default_reconciliation_interval_secs")]
    pub interval_secs: u64,
    /// Maximum GitHub API calls per minute across all candidates. Default: 20.
    #[serde(default = "default_reconciliation_max_gh_calls_per_minute")]
    pub max_gh_calls_per_minute: u32,
    /// Minimum age, in seconds, before a `github_issue_pr` workflow in
    /// `ready_to_merge` is reconciled against GitHub. Default: 300.
    #[serde(default = "default_ready_to_merge_min_age_secs")]
    pub ready_to_merge_min_age_secs: u64,
    /// Age, in seconds, after which a still-open `ready_to_merge` PR is
    /// reported as stale during reconciliation. Default: 3600.
    #[serde(default = "default_ready_to_merge_alert_ttl_secs")]
    pub ready_to_merge_alert_ttl_secs: u64,
}

fn default_reconciliation_enabled() -> bool {
    true
}

fn default_reconciliation_interval_secs() -> u64 {
    300
}

fn default_reconciliation_max_gh_calls_per_minute() -> u32 {
    20
}

fn default_ready_to_merge_min_age_secs() -> u64 {
    300
}

fn default_ready_to_merge_alert_ttl_secs() -> u64 {
    3600
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            enabled: default_reconciliation_enabled(),
            interval_secs: default_reconciliation_interval_secs(),
            max_gh_calls_per_minute: default_reconciliation_max_gh_calls_per_minute(),
            ready_to_merge_min_age_secs: default_ready_to_merge_min_age_secs(),
            ready_to_merge_alert_ttl_secs: default_ready_to_merge_alert_ttl_secs(),
        }
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
    fn reconciliation_config_defaults_include_ready_to_merge_age_limits() {
        let cfg = ReconciliationConfig::default();
        assert_eq!(cfg.ready_to_merge_min_age_secs, 300);
        assert_eq!(cfg.ready_to_merge_alert_ttl_secs, 3600);
    }

    #[test]
    fn reconciliation_config_toml_overrides_ready_to_merge_age_limits() {
        let toml = r#"
            ready_to_merge_min_age_secs = 42
            ready_to_merge_alert_ttl_secs = 84
        "#;
        let cfg: ReconciliationConfig =
            toml::from_str(toml).unwrap_or_else(|err| panic!("toml parse failed: {err}"));
        assert_eq!(cfg.ready_to_merge_min_age_secs, 42);
        assert_eq!(cfg.ready_to_merge_alert_ttl_secs, 84);
    }
}
