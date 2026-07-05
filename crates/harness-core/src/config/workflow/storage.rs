use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStoragePolicy {
    #[serde(default = "default_workflow_schema_namespace")]
    pub schema_namespace: String,
    /// Whether the background orphan-path-schema reaper runs. Bounds Postgres
    /// catalog growth automatically by dropping path-derived schemas whose
    /// owning workspace directory has been removed.
    #[serde(default = "default_true")]
    pub orphan_reaper_enabled: bool,
    /// Interval, in seconds, between background orphan-schema reap passes.
    #[serde(default = "default_orphan_reaper_interval_secs")]
    pub orphan_reaper_interval_secs: u64,
    /// Whether the orphan reaper also scans legacy unregistered path-derived
    /// schemas by subtracting live workspace-derived schema hashes.
    #[serde(default = "default_true")]
    pub orphan_reaper_legacy_enabled: bool,
    /// Maximum legacy unregistered path-derived schemas to drop per tick.
    #[serde(default = "default_orphan_reaper_legacy_batch")]
    pub orphan_reaper_legacy_batch: usize,
    /// Enable workflow-runtime stuck-instance reporting. Off by default so
    /// existing deployments keep their current background behavior on upgrade.
    #[serde(default)]
    pub workflow_watchdog_enabled: bool,
    /// Minimum age, in minutes, before blocked/awaiting-feedback workflows are
    /// reported as stuck.
    #[serde(default = "default_workflow_watchdog_age_minutes")]
    pub workflow_watchdog_age_minutes: u64,
    /// Interval, in seconds, between workflow watchdog scans.
    #[serde(default = "default_workflow_watchdog_interval_secs")]
    pub workflow_watchdog_interval_secs: u64,
    /// Maximum stuck workflow rows scanned/logged per watchdog tick.
    #[serde(default = "default_workflow_watchdog_batch_size")]
    pub workflow_watchdog_batch_size: u32,
    /// Enable pruning of terminal workflow-runtime history. Off by default
    /// because deleted runtime history is not recoverable.
    #[serde(default)]
    pub runtime_retention_enabled: bool,
    /// Terminal workflow families older than this many days are eligible for
    /// retention pruning.
    #[serde(default = "default_runtime_retention_days")]
    pub runtime_retention_days: u64,
    /// Maximum terminal root families pruned per retention tick.
    #[serde(default = "default_runtime_retention_batch_size")]
    pub runtime_retention_batch_size: u32,
    /// Interval, in seconds, between runtime-retention prune passes.
    #[serde(default = "default_runtime_retention_interval_secs")]
    pub runtime_retention_interval_secs: u64,
    /// Enable pruning of terminal task rows and task-owned child rows. Off by
    /// default because deleted task history is not recoverable.
    #[serde(default)]
    pub task_retention_enabled: bool,
    /// Terminal tasks older than this many days are eligible for retention
    /// pruning.
    #[serde(default = "default_task_retention_days")]
    pub task_retention_days: u64,
    /// Maximum terminal tasks pruned per task-retention tick.
    #[serde(default = "default_task_retention_batch_size")]
    pub task_retention_batch_size: u32,
    /// Interval, in seconds, between task-retention prune passes.
    #[serde(default = "default_task_retention_interval_secs")]
    pub task_retention_interval_secs: u64,
}

impl Default for WorkflowStoragePolicy {
    fn default() -> Self {
        Self {
            schema_namespace: default_workflow_schema_namespace(),
            orphan_reaper_enabled: true,
            orphan_reaper_interval_secs: default_orphan_reaper_interval_secs(),
            orphan_reaper_legacy_enabled: true,
            orphan_reaper_legacy_batch: default_orphan_reaper_legacy_batch(),
            workflow_watchdog_enabled: false,
            workflow_watchdog_age_minutes: default_workflow_watchdog_age_minutes(),
            workflow_watchdog_interval_secs: default_workflow_watchdog_interval_secs(),
            workflow_watchdog_batch_size: default_workflow_watchdog_batch_size(),
            runtime_retention_enabled: false,
            runtime_retention_days: default_runtime_retention_days(),
            runtime_retention_batch_size: default_runtime_retention_batch_size(),
            runtime_retention_interval_secs: default_runtime_retention_interval_secs(),
            task_retention_enabled: false,
            task_retention_days: default_task_retention_days(),
            task_retention_batch_size: default_task_retention_batch_size(),
            task_retention_interval_secs: default_task_retention_interval_secs(),
        }
    }
}

fn default_workflow_schema_namespace() -> String {
    "workflow".to_string()
}

fn default_orphan_reaper_interval_secs() -> u64 {
    3600
}

fn default_orphan_reaper_legacy_batch() -> usize {
    crate::db_pg_schema_registry::DEFAULT_ORPHAN_REAPER_LEGACY_BATCH
}

fn default_workflow_watchdog_age_minutes() -> u64 {
    240
}

fn default_workflow_watchdog_interval_secs() -> u64 {
    300
}

fn default_workflow_watchdog_batch_size() -> u32 {
    100
}

fn default_runtime_retention_days() -> u64 {
    30
}

fn default_runtime_retention_batch_size() -> u32 {
    1_000
}

fn default_runtime_retention_interval_secs() -> u64 {
    3_600
}

fn default_task_retention_days() -> u64 {
    30
}

fn default_task_retention_batch_size() -> u32 {
    1_000
}

fn default_task_retention_interval_secs() -> u64 {
    3_600
}

fn default_true() -> bool {
    true
}
