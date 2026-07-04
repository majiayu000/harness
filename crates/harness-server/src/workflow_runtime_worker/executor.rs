use crate::http::AppState;
use async_trait::async_trait;
use harness_core::config::workflow::{RuntimeDispatchProfileOverride, WorkflowConfig};
use harness_core::types::{AgentId, ThreadId};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, RuntimeJob, RuntimeJobExecutor, RuntimeKind, RuntimeProfile,
    WorkflowInstance,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use super::activity_result::activity_result_from_turn;
use super::child_workflow::execute_start_child_workflow;
use super::data_helpers::{
    activity_name, is_builtin_lifecycle_activity, prompt_payload_unavailable_result,
    prompt_task_request_for_job, PromptTaskRequest,
};
use super::merge_completion::verify_merge_completion_if_needed;
use super::pr_feedback_inspection::{
    execute_pr_feedback_inspection, is_server_owned_pr_feedback_inspection,
};
use super::prompt_input_telemetry::{
    execution_phase_for_runtime_activity, record_runtime_prompt_input,
};
use super::prompt_packet::{
    build_runtime_job_prompt, build_runtime_prompt_packet, prompt_packet_digest,
};
use super::runtime_profile::{
    agent_name_for_runtime_kind, runtime_profile_approval_policy, runtime_profile_for_job,
    runtime_profile_sandbox_mode,
};
use super::server_merge::{execute_server_merge, server_merge_execution_enabled};
use super::turn_engine::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use super::workspace::{finish_runtime_workspace, prepare_runtime_workspace};

const DEFAULT_RUNTIME_TURN_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_PR_FEEDBACK_INSPECT_TIMEOUT_SECS: u64 = 3600;
const RUNTIME_WORKSPACE_FINALIZATION_WARNING_ARTIFACT: &str =
    "runtime_workspace_finalization_warning";

pub(super) struct ServerRuntimeJobExecutor<'a> {
    state: &'a Arc<AppState>,
}

impl<'a> ServerRuntimeJobExecutor<'a> {
    pub(super) fn new(state: &'a Arc<AppState>) -> Self {
        Self { state }
    }

    async fn execute_inner(&self, job: RuntimeJob) -> anyhow::Result<ActivityResult> {
        let workflow = self.workflow_for_job(&job).await?;
        if let Some(workflow) = workflow.as_ref() {
            if workflow.is_terminal() {
                return Ok(ActivityResult::cancelled(
                    activity_name(&job),
                    format!(
                        "Workflow {} was already terminal ({}) before runtime execution.",
                        workflow.id, workflow.state
                    ),
                ));
            }
        }
        if let Some(result) = self
            .execute_server_owned_activity(&job, workflow.as_ref())
            .await?
        {
            return Ok(result);
        }
        let source_project_root = self.project_root_for_job(&job, workflow.as_ref())?;
        let workflow_document =
            harness_core::config::workflow::load_workflow_document(&source_project_root)?;
        let agent_name = agent_name_for_runtime_kind(job.runtime_kind)?;
        if self
            .state
            .core
            .server
            .agent_registry
            .get(agent_name)
            .is_none()
        {
            anyhow::bail!("runtime agent `{agent_name}` is not registered");
        }

        let runtime_profile = runtime_profile_with_timeout_fallback(
            runtime_profile_for_job(&job)?,
            &workflow_document.config,
            workflow.as_ref(),
            &job,
        );
        let sandbox_mode = runtime_profile_sandbox_mode(&runtime_profile)?;
        let approval_policy = runtime_profile_approval_policy(&runtime_profile, job.runtime_kind)?;
        let prompt_task_request =
            prompt_task_request_for_job(&job, self.state.core.workflow_runtime_store.as_deref())
                .await?;
        if let PromptTaskRequest::PayloadUnavailable { prompt_ref } = &prompt_task_request {
            return Ok(prompt_payload_unavailable_result(&job, prompt_ref));
        }

        let runtime_workspace = prepare_runtime_workspace(
            self.state,
            &job,
            workflow.as_ref(),
            &source_project_root,
            &workflow_document,
        )
        .await?;
        let activity_result: anyhow::Result<ActivityResult> = async {
            let project_root = runtime_workspace.run_project.clone();
            let prompt_packet = build_runtime_prompt_packet(
                &job,
                workflow.as_ref(),
                &project_root,
                &source_project_root,
                &runtime_profile,
                &workflow_document,
            );
            let prompt_packet_digest = prompt_packet_digest(&prompt_packet);
            self.record_prompt_packet_prepared(&job, &prompt_packet, &prompt_packet_digest)
                .await?;
            let prompt =
                build_runtime_job_prompt(&prompt_packet, prompt_task_request.prompt_text());
            let activity = activity_name(&job);
            let execution_phase = execution_phase_for_runtime_activity(&activity);
            record_runtime_prompt_input(
                self.state,
                &job,
                agent_name,
                &project_root,
                &activity,
                execution_phase,
                &prompt,
            )
            .await;
            let thread_id = self
                .state
                .core
                .server
                .thread_manager
                .start_thread(project_root.clone());
            let turn_id = self.state.core.server.thread_manager.start_turn(
                &thread_id,
                prompt.clone(),
                AgentId::from_str(agent_name),
            )?;
            persist_created_thread(self.state, &thread_id).await;

            run_turn_lifecycle_with_options(
                self.state.core.server.clone(),
                self.state.core.thread_db.clone(),
                self.state.notifications.notify_tx.clone(),
                self.state.notifications.notification_tx.clone(),
                thread_id.clone(),
                turn_id.clone(),
                prompt,
                agent_name.to_string(),
                TurnLifecycleOptions {
                    model: runtime_profile.model.clone(),
                    reasoning_effort: runtime_profile.reasoning_effort.clone(),
                    execution_phase,
                    sandbox_mode,
                    approval_policy,
                    timeout_secs: runtime_profile.timeout_secs,
                    stall_timeout_secs: None,
                    // Always drive Codex turns through the `codex exec` CLI rather
                    // than the persistent `codex app-server` JSON-RPC adapter. The
                    // app-server holds one long-lived connection per turn, which is
                    // dropped mid-stream on long (xhigh, 100+ item) turns and surfaces
                    // as "Reconnecting... N/5" failures; `codex exec` opens a fresh
                    // short-lived connection per turn and does not hit this. Both
                    // Codex runtime kinds (exec and the legacy jsonrpc label) now
                    // resolve to the CLI exec path.
                    force_code_agent: matches!(
                        job.runtime_kind,
                        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
                    ),
                },
            )
            .await;

            let turn = self
                .state
                .core
                .server
                .thread_manager
                .get_turn(&thread_id, &turn_id)
                .ok_or_else(|| anyhow::anyhow!("runtime turn disappeared before completion"))?;
            let result = activity_result_from_turn(
                &job,
                &turn.status,
                &turn.items,
                &thread_id,
                &turn_id,
                agent_name,
                &project_root,
                &prompt_packet_digest,
            );
            Ok(
                verify_merge_completion_if_needed(self.state, &job, workflow.as_ref(), result)
                    .await,
            )
        }
        .await;
        let finish_result = finish_runtime_workspace(self.state, &runtime_workspace).await;
        let activity_completed = activity_result.is_ok();
        if let Err(error) = &finish_result {
            if activity_completed {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %runtime_workspace.run_project.display(),
                    "runtime workspace finalization failed: {error}"
                );
            } else {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %runtime_workspace.run_project.display(),
                    "runtime workspace finalization failed after runtime error: {error}"
                );
            }
        }
        combine_activity_result_with_runtime_workspace_finalization(activity_result, finish_result)
    }

    async fn workflow_for_job(&self, job: &RuntimeJob) -> anyhow::Result<Option<WorkflowInstance>> {
        let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(store) = self.state.core.workflow_runtime_store.as_ref() else {
            return Ok(None);
        };
        store.get_instance(workflow_id).await
    }

    async fn execute_server_owned_activity(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<Option<ActivityResult>> {
        match activity_name(job).as_str() {
            "start_child_workflow" => Ok(Some(
                execute_start_child_workflow(self.state, job, parent).await?,
            )),
            activity if activity == harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY => Ok(
                Some(execute_pr_feedback_inspection(self.state, job, parent).await),
            ),
            "merge_pr" if server_merge_execution_enabled(self.state, job, parent) => {
                Ok(Some(execute_server_merge(self.state, job, parent).await))
            }
            _ => Ok(None),
        }
    }

    async fn record_prompt_packet_prepared(
        &self,
        job: &RuntimeJob,
        prompt_packet: &Value,
        prompt_packet_digest: &str,
    ) -> anyhow::Result<()> {
        let Some(store) = self.state.core.workflow_runtime_store.as_ref() else {
            return Ok(());
        };
        store
            .record_runtime_event(
                &job.id,
                "RuntimePromptPrepared",
                json!({
                    "prompt_packet_digest": prompt_packet_digest,
                    "prompt_packet": prompt_packet,
                }),
            )
            .await?;
        Ok(())
    }

    fn project_root_for_job(
        &self,
        job: &RuntimeJob,
        workflow: Option<&WorkflowInstance>,
    ) -> anyhow::Result<std::path::PathBuf> {
        if let Some(project_id) = workflow
            .and_then(|workflow| workflow.data.get("project_id"))
            .and_then(Value::as_str)
            .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        {
            let project_root = std::path::PathBuf::from(project_id);
            if project_root.exists() {
                return Ok(project_root);
            }
            anyhow::bail!(
                "workflow project_id path is not resolvable: {}",
                project_root.display()
            );
        }
        Ok(self.state.core.project_root.clone())
    }

    async fn runtime_worker_disabled_result(&self, job: &RuntimeJob) -> Option<ActivityResult> {
        let activity = activity_name(job);
        // If any preflight helper fails (e.g. a transient database error or a
        // missing project root), defer to the main `execute` path rather than
        // permanently failing the job here. That path classifies transient vs
        // fatal errors and applies the retry policy; a hard failure in preflight
        // would bypass it.
        let workflow = self.workflow_for_job(job).await.ok()?;
        let source_project_root = self.project_root_for_job(job, workflow.as_ref()).ok()?;
        let workflow_document =
            harness_core::config::workflow::load_workflow_document(&source_project_root).ok()?;
        runtime_worker_disabled_result_for_config(
            &activity,
            &source_project_root,
            &workflow_document.config,
        )
    }
}

#[async_trait]
impl RuntimeJobExecutor for ServerRuntimeJobExecutor<'_> {
    fn consumes_runtime_turn(&self, job: &RuntimeJob) -> bool {
        !is_internal_non_agent_activity(job)
    }

    async fn preflight_result(&self, job: &RuntimeJob) -> Option<ActivityResult> {
        // Internal server-owned activities do not run a user agent. They must keep
        // flowing even when the runtime worker is disabled, otherwise disabling the
        // worker would strand workflows or prevent server-owned PR snapshots.
        if is_internal_non_agent_activity(job) {
            return None;
        }
        self.runtime_worker_disabled_result(job).await
    }

    async fn execute(&self, job: RuntimeJob) -> ActivityResult {
        let activity = activity_name(&job);
        match self.execute_inner(job).await {
            Ok(result) => result,
            Err(error) => ActivityResult::failed(
                activity,
                "Runtime job execution failed before the agent completed.",
                error.to_string(),
            ),
        }
    }
}

fn is_internal_non_agent_activity(job: &RuntimeJob) -> bool {
    is_builtin_lifecycle_activity(job) || is_server_owned_pr_feedback_inspection(job)
}

async fn persist_created_thread(state: &AppState, thread_id: &ThreadId) {
    let Some(db) = state.core.thread_db.as_ref() else {
        return;
    };
    let Some(thread) = state.core.server.thread_manager.get_thread(thread_id) else {
        return;
    };
    if let Err(error) = db.insert(&thread).await {
        tracing::warn!(thread_id = %thread_id, "failed to persist runtime worker thread: {error}");
    }
}

fn runtime_profile_with_timeout_fallback(
    mut profile: RuntimeProfile,
    workflow_config: &WorkflowConfig,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> RuntimeProfile {
    if profile.timeout_secs.is_none() {
        profile.timeout_secs = runtime_timeout_fallback(workflow_config, workflow, job);
    }
    profile
}

fn runtime_timeout_fallback(
    workflow_config: &WorkflowConfig,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> Option<u64> {
    let runtime_dispatch = &workflow_config.runtime_dispatch;
    let workflow_definition_id = workflow.map(|workflow| workflow.definition_id.as_str());
    let activity = activity_name(job);

    workflow_definition_id
        .and_then(|definition_id| {
            runtime_dispatch
                .workflow_activity_profiles
                .get(definition_id)
                .and_then(|profiles| profiles.get(&activity))
        })
        .and_then(profile_timeout)
        .or_else(|| {
            runtime_dispatch
                .activity_profiles
                .get(&activity)
                .and_then(profile_timeout)
        })
        .or_else(|| {
            workflow_definition_id.and_then(|definition_id| {
                runtime_dispatch
                    .workflow_profiles
                    .get(definition_id)
                    .and_then(profile_timeout)
            })
        })
        .or(runtime_dispatch.timeout_secs)
        .or_else(|| default_runtime_timeout(&activity))
}

fn profile_timeout(profile: &RuntimeDispatchProfileOverride) -> Option<u64> {
    profile.timeout_secs
}

fn default_runtime_timeout(activity: &str) -> Option<u64> {
    Some(match activity {
        "inspect_pr_feedback" => DEFAULT_PR_FEEDBACK_INSPECT_TIMEOUT_SECS,
        _ => DEFAULT_RUNTIME_TURN_TIMEOUT_SECS,
    })
}

fn runtime_worker_disabled_result_for_config(
    activity: &str,
    project_root: &Path,
    workflow_config: &WorkflowConfig,
) -> Option<ActivityResult> {
    if workflow_config.runtime_worker.enabled {
        return None;
    }
    Some(ActivityResult::cancelled(
        activity,
        format!(
            "Runtime worker is disabled for project {}; claimed job was cancelled before agent execution.",
            project_root.display()
        ),
    ))
}

fn combine_activity_result_with_runtime_workspace_finalization(
    activity_result: anyhow::Result<ActivityResult>,
    finish_result: anyhow::Result<()>,
) -> anyhow::Result<ActivityResult> {
    match (activity_result, finish_result) {
        (Ok(result), Err(error)) => {
            if result.status == harness_workflow::runtime::ActivityStatus::Succeeded {
                Ok(activity_result_failed_by_runtime_workspace_finalization(
                    result, &error,
                ))
            } else {
                Ok(result.with_artifact(runtime_workspace_finalization_warning_artifact(&error)))
            }
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(finish_error)) => Err(error.context(format!(
            "runtime workspace finalization also failed: {finish_error}"
        ))),
        (Err(error), Ok(())) => Err(error),
    }
}

fn activity_result_failed_by_runtime_workspace_finalization(
    mut result: ActivityResult,
    error: &anyhow::Error,
) -> ActivityResult {
    result.status = harness_workflow::runtime::ActivityStatus::Failed;
    result.summary =
        "Runtime workspace finalization failed after the activity completed.".to_string();
    result.error = Some(format!("runtime workspace finalization failed: {error}"));
    result.error_kind = Some(harness_workflow::runtime::ActivityErrorKind::Retryable);
    result.with_artifact(runtime_workspace_finalization_warning_artifact(error))
}

fn runtime_workspace_finalization_warning_artifact(error: &anyhow::Error) -> ActivityArtifact {
    ActivityArtifact::new(
        RUNTIME_WORKSPACE_FINALIZATION_WARNING_ARTIFACT,
        json!({ "error": error.to_string() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject};
    use serde_json::json;

    fn runtime_job(activity: &str) -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": activity }),
        )
    }

    fn workflow(definition_id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition_id,
            1,
            "running",
            WorkflowSubject::new("issue", "issue-1"),
        )
    }

    #[test]
    fn runtime_timeout_fallback_prefers_workflow_activity_profile() {
        let mut config = WorkflowConfig::default();
        config.runtime_dispatch.timeout_secs = Some(900);
        config.runtime_dispatch.activity_profiles.insert(
            "inspect_pr_feedback".to_string(),
            RuntimeDispatchProfileOverride {
                timeout_secs: Some(1800),
                ..RuntimeDispatchProfileOverride::default()
            },
        );
        config.runtime_dispatch.workflow_profiles.insert(
            "pr_feedback".to_string(),
            RuntimeDispatchProfileOverride {
                timeout_secs: Some(240),
                ..RuntimeDispatchProfileOverride::default()
            },
        );
        config
            .runtime_dispatch
            .workflow_activity_profiles
            .entry("pr_feedback".to_string())
            .or_default()
            .insert(
                "inspect_pr_feedback".to_string(),
                RuntimeDispatchProfileOverride {
                    timeout_secs: Some(120),
                    ..RuntimeDispatchProfileOverride::default()
                },
            );

        assert_eq!(
            runtime_timeout_fallback(
                &config,
                Some(&workflow("pr_feedback")),
                &runtime_job("inspect_pr_feedback"),
            ),
            Some(120)
        );
    }

    #[test]
    fn runtime_profile_with_timeout_fallback_preserves_embedded_timeout() {
        let mut config = WorkflowConfig::default();
        config.runtime_dispatch.timeout_secs = Some(900);
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        profile.timeout_secs = Some(42);

        let profile = runtime_profile_with_timeout_fallback(
            profile,
            &config,
            Some(&workflow("pr_feedback")),
            &runtime_job("inspect_pr_feedback"),
        );

        assert_eq!(profile.timeout_secs, Some(42));
    }

    #[test]
    fn runtime_timeout_fallback_has_global_activity_defaults() {
        let config = WorkflowConfig::default();

        assert_eq!(
            runtime_timeout_fallback(
                &config,
                Some(&workflow("pr_feedback")),
                &runtime_job("inspect_pr_feedback")
            ),
            Some(3600)
        );
        assert_eq!(
            runtime_timeout_fallback(&config, None, &runtime_job("implement_issue")),
            Some(3600)
        );
    }

    #[test]
    fn runtime_worker_disabled_result_for_config_cancels_agent_work() {
        let mut config = WorkflowConfig::default();
        config.runtime_worker.enabled = false;

        let Some(result) = runtime_worker_disabled_result_for_config(
            "implement_issue",
            Path::new("/tmp/project"),
            &config,
        ) else {
            panic!("disabled runtime worker should produce a preflight result");
        };

        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Cancelled
        );
        assert_eq!(result.activity, "implement_issue");
        assert!(result
            .summary
            .contains("Runtime worker is disabled for project /tmp/project"));
    }

    #[test]
    fn runtime_worker_disabled_result_for_config_allows_enabled_project() {
        let config = WorkflowConfig::default();

        assert!(runtime_worker_disabled_result_for_config(
            "implement_issue",
            Path::new("/tmp/project"),
            &config,
        )
        .is_none());
    }

    #[test]
    fn runtime_workspace_finalization_failure_marks_activity_failed() {
        let result =
            ActivityResult::succeeded("implement_issue", "Created a pull request.").with_artifact(
                ActivityArtifact::new("pull_request", json!({ "pr_number": 42 })),
            );

        let result = match combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        ) {
            Ok(result) => result,
            Err(error) => panic!(
                "finalization failure should be returned as a failed activity result: {error}"
            ),
        };

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Retryable)
        );
        assert!(result
            .summary
            .contains("Runtime workspace finalization failed"));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("after_run hook failed"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        let Some(warning) = result.artifacts.iter().find(|artifact| {
            artifact.artifact_type == RUNTIME_WORKSPACE_FINALIZATION_WARNING_ARTIFACT
        }) else {
            panic!("finalization failure should preserve a diagnostic warning artifact");
        };
        assert_eq!(warning.artifact["error"], "after_run hook failed");
    }

    #[test]
    fn runtime_workspace_finalization_failure_preserves_failed_activity_result() {
        let result = ActivityResult::failed(
            "address_pr_feedback",
            "Structured output was invalid.",
            "fatal",
        )
        .with_error_kind(harness_workflow::runtime::ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            "activity_result_parse_error",
            json!({ "field": "status" }),
        ));

        let result = match combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        ) {
            Ok(result) => result,
            Err(error) => panic!("failed activity result should be preserved: {error}"),
        };

        assert_eq!(result.activity, "address_pr_feedback");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(result.summary, "Structured output was invalid.");
        assert_eq!(result.error.as_deref(), Some("fatal"));
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Fatal)
        );
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "activity_result_parse_error"));
        let Some(warning) = result.artifacts.iter().find(|artifact| {
            artifact.artifact_type == RUNTIME_WORKSPACE_FINALIZATION_WARNING_ARTIFACT
        }) else {
            panic!("finalization failure should preserve a diagnostic warning artifact");
        };
        assert_eq!(warning.artifact["error"], "after_run hook failed");
    }

    #[test]
    fn internal_non_agent_activity_includes_server_owned_pr_inspection() {
        assert!(is_internal_non_agent_activity(&runtime_job(
            "inspect_pr_feedback"
        )));
        assert!(!is_internal_non_agent_activity(&runtime_job(
            "implement_issue"
        )));
    }
}
