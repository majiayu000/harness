pub(crate) mod agent_review;
mod agent_review_provider_gate;
#[cfg(test)]
mod agent_review_tests;
pub(crate) mod conflict_resolver;
pub(crate) mod gates;
pub(crate) mod helpers;
pub(crate) mod implement_pipeline;
mod local_review_completion;
#[cfg(test)]
mod local_review_completion_gate_tests;
#[cfg(test)]
mod local_review_completion_tests;
pub(crate) mod non_implementation;
pub(crate) mod pr_detection;
pub(crate) mod review_loop;
#[cfg(test)]
mod review_loop_wait_budget_tests;
mod run_task;
pub(crate) mod triage_pipeline;
pub(crate) mod turn_lifecycle;
#[cfg(test)]
mod turn_lifecycle_stall_tests;
#[cfg(test)]
mod turn_lifecycle_terminal_error_tests;
mod validation_gate;

use harness_core::{config::agents::CapabilityProfile, interceptor::TurnInterceptor};
use std::sync::Arc;

#[cfg(test)]
use crate::task_runner::{mutate_and_persist, CreateTaskRequest, TaskId, TaskStatus, TaskStore};
#[cfg(test)]
use local_review_completion::{
    complete_after_local_review_without_hosted_bot, fail_missing_local_review_gate,
    fail_review_provider_gate, LocalReviewPrChecks, LocalReviewPrHead, LocalReviewPrState,
    LocalReviewReadyToMergeFeedback,
};
#[cfg(test)]
use pr_detection::{
    build_fix_ci_prompt, parse_harness_mention_command, HarnessMentionCommand, PromptBuilder,
};
#[cfg(test)]
use run_task::{
    effective_agent_review_round_limit, effective_hosted_review_round_limit,
    initial_hosted_review_wait_secs, local_review_pr_check_timeout_secs, review_repo_slug,
    should_run_issue_triage,
};
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use tokio::time::Instant;

use run_task::run_task as run_task_impl;
use turn_lifecycle::run_turn_lifecycle as run_turn_lifecycle_impl;
// Wrappers keep existing call sites stable while counting old task surfaces.
pub(crate) async fn run_task(
    store: &crate::task_runner::TaskStore,
    task_id: &crate::task_runner::TaskId,
    agent: &dyn harness_core::agent::CodeAgent,
    reviewer: Option<&dyn harness_core::agent::CodeAgent>,
    skills: Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: SharedTurnInterceptors,
    req: &crate::task_runner::CreateTaskRequest,
    project: std::path::PathBuf,
    project_root: std::path::PathBuf,
    server_config: &harness_core::config::HarnessConfig,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    turns_used_acc: &mut u32,
) -> anyhow::Result<()> {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskExecutor,
    );
    run_task_impl(
        store,
        task_id,
        agent,
        reviewer,
        skills,
        events,
        interceptors,
        req,
        project,
        project_root,
        server_config,
        issue_workflow_store,
        workflow_runtime_store,
        turns_used_acc,
    )
    .await
}

pub(crate) async fn run_turn_lifecycle(
    server: Arc<crate::server::HarnessServer>,
    thread_db: Option<crate::thread_db::ThreadDb>,
    notify_tx: Option<crate::notify::NotifySender>,
    notification_tx: tokio::sync::broadcast::Sender<
        harness_protocol::notifications::RpcNotification,
    >,
    thread_id: harness_core::types::ThreadId,
    turn_id: harness_core::types::TurnId,
    prompt: String,
    agent_name: String,
) {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskExecutor,
    );
    run_turn_lifecycle_impl(
        server,
        thread_db,
        notify_tx,
        notification_tx,
        thread_id,
        turn_id,
        prompt,
        agent_name,
    )
    .await;
}
pub(crate) use validation_gate::run_test_gate;

pub(crate) type TurnInterceptorHandle = Arc<dyn TurnInterceptor>;
pub(crate) type SharedTurnInterceptors = Arc<[TurnInterceptorHandle]>;

/// Extract tool list from a capability profile, returning an error if the
/// profile unexpectedly returns `None` (which means Full/unrestricted).
/// A misconfigured profile causes a hard failure rather than silent degradation,
/// per U-23 (no silent capability downgrade).
pub(crate) fn restricted_tools(profile: CapabilityProfile) -> anyhow::Result<Vec<String>> {
    profile.tools().ok_or_else(|| {
        anyhow::anyhow!(
            "capability profile {:?} returned None from tools() — misconfiguration",
            profile
        )
    })
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
