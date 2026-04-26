use crate::types::Grade;
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Maximum number of tasks executing concurrently across all projects. Default: 4.
    #[serde(default = "default_max_concurrent_tasks")]
    pub max_concurrent_tasks: usize,
    /// Maximum number of tasks waiting for a slot. Excess tasks are rejected. Default: 32.
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    /// Seconds of silence from the agent stream before declaring a stall. Default: 300.
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
    300
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveConfig {
    pub session_renewal_secs: u64,
    pub log_retention_days: u32,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
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
/// via `gh`, and applies the appropriate task-status transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationConfig {
    /// Whether the reconciliation loop is enabled. Default: true.
    #[serde(default = "default_reconciliation_enabled")]
    pub enabled: bool,
    /// Seconds between reconciliation ticks. Default: 300 (5 minutes).
    #[serde(default = "default_reconciliation_interval_secs")]
    pub interval_secs: u64,
    /// Maximum `gh` CLI calls per minute across all candidates. Default: 20.
    #[serde(default = "default_reconciliation_max_gh_calls_per_minute")]
    pub max_gh_calls_per_minute: u32,
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

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            enabled: default_reconciliation_enabled(),
            interval_secs: default_reconciliation_interval_secs(),
            max_gh_calls_per_minute: default_reconciliation_max_gh_calls_per_minute(),
        }
    }
}

/// Daily maintenance window configuration for scheduled restarts and self-updates.
///
/// When enabled, no new tasks are dispatched during the configured time window.
/// In-flight tasks continue to completion. The window boundary logic handles
/// cross-midnight ranges (e.g. 23:00–02:00) and DST transitions via `chrono-tz`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceWindowConfig {
    /// When false (default), this feature is completely disabled. No behaviour change.
    #[serde(default)]
    pub enabled: bool,
    /// Start of the quiet window in local time (inclusive). Format: "HH:MM:SS".
    #[serde(default = "default_quiet_window_start")]
    pub quiet_window_start: NaiveTime,
    /// End of the quiet window in local time (exclusive). Format: "HH:MM:SS".
    /// If `quiet_window_end < quiet_window_start`, the window crosses midnight.
    #[serde(default = "default_quiet_window_end")]
    pub quiet_window_end: NaiveTime,
    /// IANA timezone name (e.g. "America/New_York"). Defaults to "UTC".
    #[serde(default = "default_maintenance_timezone")]
    pub timezone: String,
    /// Maximum seconds to wait for in-flight tasks to drain before external restart.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
}

fn default_quiet_window_start() -> NaiveTime {
    // 06:00:00 — always valid; fallback to midnight if chrono invariants change.
    NaiveTime::from_hms_opt(6, 0, 0).unwrap_or(NaiveTime::MIN)
}

fn default_quiet_window_end() -> NaiveTime {
    // 08:00:00 — always valid; fallback to midnight if chrono invariants change.
    NaiveTime::from_hms_opt(8, 0, 0).unwrap_or(NaiveTime::MIN)
}

fn default_maintenance_timezone() -> String {
    "UTC".to_string()
}

fn default_drain_timeout_secs() -> u64 {
    3600
}

impl Default for MaintenanceWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            quiet_window_start: default_quiet_window_start(),
            quiet_window_end: default_quiet_window_end(),
            timezone: default_maintenance_timezone(),
            drain_timeout_secs: default_drain_timeout_secs(),
        }
    }
}

impl MaintenanceWindowConfig {
    /// Returns `true` when `now` falls inside the configured quiet window.
    ///
    /// Always returns `false` when `enabled = false`.
    /// Handles cross-midnight windows: if `quiet_window_end < quiet_window_start`,
    /// the window spans midnight and is active when `time >= start || time < end`.
    /// Equal start and end produce a zero-length window (always returns `false`).
    /// Falls back to UTC if the timezone string cannot be parsed.
    pub fn in_quiet_window(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        let tz = self
            .timezone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&tz);
        let t = local.time();
        let start = self.quiet_window_start;
        let end = self.quiet_window_end;
        if end < start {
            // Cross-midnight: active from start to midnight, and midnight to end.
            t >= start || t < end
        } else {
            t >= start && t < end
        }
    }

    /// Returns the number of seconds until the quiet window ends.
    ///
    /// Uses timezone-aware `DateTime` arithmetic so the result is correct on
    /// DST transition days. Returns 0 when `enabled = false`.
    pub fn secs_until_window_end(&self, now: DateTime<Utc>) -> u64 {
        if !self.enabled {
            return 0;
        }
        let tz = self
            .timezone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let local: chrono::DateTime<chrono_tz::Tz> = now.with_timezone(&tz);
        let today = local.date_naive();
        let end_naive = today.and_time(self.quiet_window_end);
        // earliest() resolves DST ambiguity (fall-back); returns None on spring-forward
        // gaps, which we treat as "window end has passed".
        let diff_secs = match tz.from_local_datetime(&end_naive).earliest() {
            Some(end_dt) => end_dt.signed_duration_since(local).num_seconds(),
            None => 0,
        };
        if diff_secs > 0 {
            diff_secs as u64
        } else {
            // End time has already passed today; for cross-midnight windows the end
            // falls tomorrow — compute against tomorrow's wall-clock end time.
            let tomorrow = today + Duration::days(1);
            let end_tomorrow_naive = tomorrow.and_time(self.quiet_window_end);
            tz.from_local_datetime(&end_tomorrow_naive)
                .earliest()
                .map(|end_dt| end_dt.signed_duration_since(local).num_seconds().max(0) as u64)
                .unwrap_or(0)
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

    // ── MaintenanceWindowConfig tests ────────────────────────────────────────

    fn utc_at(hour: u32, min: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2024, 3, 15, hour, min, 0)
            .single()
            .expect("valid datetime")
    }

    fn window(start_h: u32, start_m: u32, end_h: u32, end_m: u32) -> MaintenanceWindowConfig {
        MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(start_h, start_m, 0).unwrap(),
            quiet_window_end: NaiveTime::from_hms_opt(end_h, end_m, 0).unwrap(),
            timezone: "UTC".to_string(),
            drain_timeout_secs: 3600,
        }
    }

    #[test]
    fn test_disabled_always_false() {
        let mut cfg = window(6, 0, 8, 0);
        cfg.enabled = false;
        // Any time should return false when disabled.
        assert!(!cfg.in_quiet_window(utc_at(7, 0)));
        assert!(!cfg.in_quiet_window(utc_at(6, 0)));
    }

    #[test]
    fn test_inside_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(cfg.in_quiet_window(utc_at(7, 0)));
    }

    #[test]
    fn test_before_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(5, 59)));
    }

    #[test]
    fn test_after_window() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(8, 1)));
    }

    #[test]
    fn test_boundary_start_inclusive() {
        let cfg = window(6, 0, 8, 0);
        assert!(cfg.in_quiet_window(utc_at(6, 0)));
    }

    #[test]
    fn test_boundary_end_exclusive() {
        let cfg = window(6, 0, 8, 0);
        assert!(!cfg.in_quiet_window(utc_at(8, 0)));
    }

    #[test]
    fn test_cross_midnight_inside() {
        // Window 23:00–02:00, now 00:30 → inside.
        let cfg = window(23, 0, 2, 0);
        assert!(cfg.in_quiet_window(utc_at(0, 30)));
    }

    #[test]
    fn test_cross_midnight_outside() {
        // Window 23:00–02:00, now 12:00 → outside.
        let cfg = window(23, 0, 2, 0);
        assert!(!cfg.in_quiet_window(utc_at(12, 0)));
    }

    #[test]
    fn test_dst_spring_forward_no_panic() {
        // America/New_York spring-forward 2024-03-10 02:00 → 03:00.
        // 02:30 local is a gap — must not panic.
        use chrono::TimeZone;
        let cfg = MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(2, 0, 0).unwrap(),
            quiet_window_end: NaiveTime::from_hms_opt(4, 0, 0).unwrap(),
            timezone: "America/New_York".to_string(),
            drain_timeout_secs: 3600,
        };
        // 07:30 UTC = 02:30 EST (before spring forward), which is in the gap.
        let now = Utc
            .with_ymd_and_hms(2024, 3, 10, 7, 30, 0)
            .single()
            .unwrap();
        // Should not panic; result can be true or false depending on gap handling.
        cfg.in_quiet_window(now);
    }

    #[test]
    fn test_dst_fall_back_no_panic() {
        // America/New_York fall-back 2024-11-03 02:00 → 01:00 (ambiguous).
        use chrono::TimeZone;
        let cfg = MaintenanceWindowConfig {
            enabled: true,
            quiet_window_start: NaiveTime::from_hms_opt(1, 0, 0).unwrap_or(NaiveTime::MIN),
            quiet_window_end: NaiveTime::from_hms_opt(3, 0, 0).unwrap_or(NaiveTime::MIN),
            timezone: "America/New_York".to_string(),
            drain_timeout_secs: 3600,
        };
        // 06:30 UTC = 01:30 EST (ambiguous on fall-back day).
        let now = Utc
            .with_ymd_and_hms(2024, 11, 3, 6, 30, 0)
            .single()
            .unwrap();
        // Should not panic; result can be true or false depending on ambiguity handling.
        cfg.in_quiet_window(now);
    }

    #[test]
    fn test_secs_until_end_normal() {
        let cfg = window(6, 0, 8, 0);
        // At 07:00, window ends at 08:00 → 3600 seconds.
        let secs = cfg.secs_until_window_end(utc_at(7, 0));
        assert_eq!(secs, 3600);
    }

    #[test]
    fn test_secs_until_end_cross_midnight() {
        // Window 23:00–02:00, now 23:30. End is 02:00 next day → 2.5h = 9000s.
        let cfg = window(23, 0, 2, 0);
        let secs = cfg.secs_until_window_end(utc_at(23, 30));
        assert_eq!(secs, 9000);
    }

    #[test]
    fn test_maintenance_window_config_default_disabled() {
        let cfg = MaintenanceWindowConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.timezone, "UTC");
        assert_eq!(cfg.drain_timeout_secs, 3600);
    }

    #[test]
    fn test_maintenance_window_config_toml_roundtrip() {
        let toml = r#"
            enabled = true
            quiet_window_start = "06:00:00"
            quiet_window_end = "08:00:00"
            timezone = "America/New_York"
            drain_timeout_secs = 7200
        "#;
        let cfg: MaintenanceWindowConfig = toml::from_str(toml).expect("toml parse failed");
        assert!(cfg.enabled);
        assert_eq!(cfg.timezone, "America/New_York");
        assert_eq!(cfg.drain_timeout_secs, 7200);
    }
}
