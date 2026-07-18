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
    RuntimeProfile, RuntimeProfileSelector, WorkflowCommandRecord, WorkflowCommandType,
    WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore,
};
use sha2::{Digest, Sha256};

mod auto_recovery;
mod pr_feedback;
mod runtime_command_dispatch;
mod runtime_profiles;
mod runtime_workers;
use runtime_command_dispatch::workflow_project_root;
use runtime_profiles::{
    persist_runtime_profile_manifest, runtime_default_profile_for_project,
    runtime_dispatch_profile_selector,
};
const RUNTIME_WORKFLOW_CONFIG_RETRY_SECS: u64 = 30;

pub(super) use auto_recovery::spawn_auto_recovery;
#[cfg(test)]
pub(super) use pr_feedback::run_runtime_pr_feedback_sweep_tick;
pub(super) use pr_feedback::spawn_runtime_pr_feedback_sweeper;
#[cfg(test)]
pub(super) use runtime_command_dispatch::run_runtime_command_dispatch_tick;
pub(super) use runtime_command_dispatch::{
    github_repo_project_root, load_runtime_workflow_config, spawn_runtime_command_dispatcher,
};
#[cfg(test)]
pub(super) use runtime_profiles::runtime_profile_manifest_definition;
pub(super) use runtime_workers::spawn_runtime_job_workers;
