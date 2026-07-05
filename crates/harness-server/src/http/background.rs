use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::auto_merge::{
    expected_base_ref_from_workflow_data, prepare_auto_merge_workflow_from_snapshot,
    AutoMergeSnapshotGate,
};
use super::{state::AppState, task_routes};
use crate::github_pr_snapshot::{fetch_github_pr_snapshot, GitHubPrSnapshotTarget};
use crate::task_runner;
use anyhow::Context;
use harness_workflow::issue_lifecycle::{
    is_feedback_claim_placeholder, IssueLifecycleState, IssueWorkflowInstance,
};
use harness_workflow::runtime::{
    CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeKind, RuntimeProfile,
    RuntimeProfileSelector, WorkflowCommandRecord, WorkflowCommandStatus, WorkflowCommandType,
    WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore,
};
use sha2::{Digest, Sha256};

mod awaiting_deps;
mod checkpoint_recovery;
mod orphan_pending_recovery;
mod pr_feedback;
mod pr_recovery;
mod runtime_command_dispatch;
mod runtime_profiles;
mod runtime_workers;
mod support;
mod system_recovery;

#[cfg(test)]
mod tests;

#[cfg(test)]
use pr_recovery::{task_is_pr_recovery_candidate, workflow_recovery_task_ids};
use runtime_command_dispatch::workflow_project_root;
use runtime_profiles::{
    persist_runtime_profile_manifest, runtime_default_profile_for_project,
    runtime_dispatch_profile_selector,
};
#[cfg(test)]
use runtime_profiles::{runtime_dispatch_profile, runtime_profile_from_agent};
#[cfg(test)]
use runtime_workers::{
    drain_finished_runtime_worker_ticks, runtime_worker_has_external_state_owner,
    runtime_worker_loop_policy, runtime_worker_open_slots, runtime_worker_sleep_or_shutdown,
};
#[cfg(test)]
use support::review_config_for_request;
use support::{
    await_startup_recovery_ready_task, build_recovered_request, fail_background_dispatch_task,
    orphan_recovery_failure_reason, parse_issue_pr, resolve_reviewer_for_request,
    task_allows_prompt_orphan_recovery, RUNTIME_WORKFLOW_CONFIG_RETRY_SECS,
};

pub(super) use awaiting_deps::spawn_awaiting_deps_watcher;
pub(super) use checkpoint_recovery::spawn_checkpoint_recovery;
pub(super) use orphan_pending_recovery::spawn_orphan_pending_recovery;
#[cfg(test)]
pub(super) use pr_feedback::run_runtime_pr_feedback_sweep_tick;
pub(super) use pr_feedback::spawn_runtime_pr_feedback_sweeper;
pub(super) use pr_recovery::spawn_pr_recovery;
#[cfg(test)]
pub(super) use runtime_command_dispatch::run_runtime_command_dispatch_tick;
pub(super) use runtime_command_dispatch::{
    github_repo_project_root, load_runtime_workflow_config, spawn_runtime_command_dispatcher,
};
#[cfg(test)]
pub(super) use runtime_profiles::runtime_profile_manifest_definition;
pub(super) use runtime_workers::spawn_runtime_job_workers;
pub(super) use support::recovery_queue_domain;
pub(super) use system_recovery::spawn_system_task_recovery;
