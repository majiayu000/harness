//! Serde `default = "..."` helper functions for workflow policy fields.
//!
//! Extracted from `workflow.rs` to keep that file within the size ceiling.
//! Glob-imported back into the parent module so existing
//! `#[serde(default = "default_x")]` attributes resolve unchanged.

use super::RuntimeDispatchProfileOverride;
use std::collections::BTreeMap;

pub(super) fn default_workflow_version() -> u32 {
    1
}

pub(super) fn default_base_remote() -> String {
    "origin".to_string()
}

pub(super) fn default_base_branch() -> String {
    "main".to_string()
}

pub(super) fn default_workspace_strategy() -> String {
    "worktree".to_string()
}

pub(super) fn default_workspace_branch_prefix() -> String {
    "harness/".to_string()
}

pub(super) fn default_workspace_cleanup() -> String {
    "on_terminal".to_string()
}

pub(super) fn default_hook_timeout_secs() -> u64 {
    60
}

pub(super) fn default_force_execute_label() -> String {
    "force-execute".to_string()
}

pub(super) fn default_pr_scope_guard_max_files_changed() -> u32 {
    30
}

pub(super) fn default_pr_scope_guard_max_lines_added() -> u32 {
    1500
}

pub(super) fn default_feedback_sweep_interval_secs() -> u64 {
    60
}

pub(super) fn default_feedback_claim_stale_after_secs() -> u64 {
    300
}

pub(super) fn default_pr_hygiene_interval_secs() -> u64 {
    30 * 60
}

pub(super) fn default_pr_hygiene_dirty_age_to_repair_secs() -> u64 {
    48 * 60 * 60
}

pub(super) fn default_pr_hygiene_dirty_age_to_comment_secs() -> u64 {
    7 * 24 * 60 * 60
}

pub(super) fn default_pr_hygiene_rebase_needed_label() -> String {
    "rebase-needed".to_string()
}

pub(super) fn default_pr_hygiene_batch_limit() -> u32 {
    25
}

pub(super) fn default_runtime_dispatch_interval_secs() -> u64 {
    30
}

pub(super) fn default_runtime_dispatch_batch_limit() -> u32 {
    25
}

pub(super) fn default_runtime_dispatch_defer_backoff_secs() -> u64 {
    30
}

pub(super) fn default_runtime_dispatch_defer_backoff_max_secs() -> u64 {
    15 * 60
}

pub(super) fn default_runtime_dispatch_activity_profiles(
) -> BTreeMap<String, RuntimeDispatchProfileOverride> {
    BTreeMap::new()
}

pub(super) fn default_runtime_worker_interval_secs() -> u64 {
    5
}

pub(super) fn default_runtime_worker_concurrency() -> u32 {
    10
}

pub(super) fn default_runtime_worker_lease_ttl_secs() -> u64 {
    3900
}

pub(super) fn default_true() -> bool {
    true
}
