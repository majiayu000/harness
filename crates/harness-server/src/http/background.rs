use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::auto_merge::{
    expected_base_ref_from_workflow_data, prepare_auto_merge_workflow_from_snapshot,
    AutoMergeSnapshotGate,
};
use super::state::AppState;
use crate::github_pr_snapshot::{fetch_github_pr_snapshot, GitHubPrSnapshotTarget};
use anyhow::Context;
use harness_workflow::runtime::{
    CommandDispatchOutcome, DeferClaimedCommandOutcome, DispatchBackoffPolicy,
    DispatchBarrierInput, DispatchBarrierReasonCode, RuntimeCommandDispatcher, RuntimeKind,
    RuntimeProfile, RuntimeProfileSelector, WorkflowCommandRecord, WorkflowDefinition,
    WorkflowInstance, WorkflowPrBindingRepairOutcome, WorkflowRuntimeStore,
};
use sha2::{Digest, Sha256};

mod auto_recovery;
pub(crate) mod loop_health;
mod pr_feedback;
mod runtime_command_dispatch;
mod runtime_profiles;
mod runtime_workers;
pub(crate) use loop_health::{BackgroundLoopHealth, LoopHandle};
use runtime_command_dispatch::workflow_project_root;
use runtime_profiles::{
    persist_runtime_profile_manifest, runtime_default_profile_for_project,
    runtime_dispatch_profile_selector, runtime_profile_with_prompt_execution_policy,
};
const RUNTIME_WORKFLOW_CONFIG_RETRY_SECS: u64 = 30;

pub(super) use auto_recovery::spawn_auto_recovery;
pub(super) use pr_feedback::spawn_runtime_pr_feedback_sweeper;
#[cfg(test)]
pub(super) use pr_feedback::{
    run_runtime_pr_feedback_sweep_tick, run_runtime_pr_feedback_sweep_tick_with_cursor,
};
#[cfg(test)]
pub(super) use runtime_command_dispatch::run_runtime_command_dispatch_tick;
pub(super) use runtime_command_dispatch::{
    github_repo_project_root, load_runtime_workflow_config, spawn_runtime_command_dispatcher,
};
#[cfg(test)]
pub(super) use runtime_profiles::runtime_profile_manifest_definition;
pub(super) use runtime_workers::spawn_runtime_job_workers;

/// Load the workflow config for a background loop, reporting parse failures
/// into the loop-health registry instead of silently degrading (GH-1880).
///
/// Every config-reload failure arm converges on this helper so a malformed
/// WORKFLOW.md surfaces once, at error level, with the affected loop names —
/// retention, watchdog, and the reaper can no longer fail silently while the
/// dispatcher keeps running.
pub(crate) async fn load_workflow_config_for_loop(
    state: &Arc<crate::http::AppState>,
    handle: &LoopHandle,
) -> anyhow::Result<harness_core::config::workflow::WorkflowConfig> {
    match harness_core::config::workflow::load_workflow_config(&state.core.project_root) {
        Ok(config) => {
            handle.tick_ok();
            // Config recovered: clear the aggregated failure so operators see
            // the loop return to a healthy state.
            state.background_loops.clear_config_failure();
            Ok(config)
        }
        Err(error) => {
            handle.config_failure(&error.to_string());
            Err(error)
        }
    }
}
