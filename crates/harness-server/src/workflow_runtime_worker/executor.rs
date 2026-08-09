use crate::http::AppState;
use harness_core::agent::AGENT_OUTPUT_SCHEMA_PATH_ENV;
use harness_core::config::workflow::WorkflowConfig;
use harness_core::types::AgentId;
use harness_workflow::runtime::{ActivityArtifact, ActivityResult, RuntimeJob, WorkflowInstance};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::activity_result::{
    activity_result_from_turn_with_workflow, structured_output_correction,
};
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
use super::repo_memory_prompt::{repo_memory_config_artifact, repo_memory_for_prompt_packet};
use super::runtime_profile::{
    agent_name_for_runtime_kind, resolve_runtime_settings, runtime_profile_for_job,
};
use super::runtime_turn_control::{force_code_agent_for_runtime_turn, RuntimeTurnAliasGuard};
use super::runtime_usage::runtime_usage_context;
use super::server_merge::{execute_server_merge, server_merge_execution_enabled};
use super::turn_engine::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use super::workspace::{finish_runtime_workspace, prepare_runtime_workspace};
#[path = "executor/runtime_timeout.rs"]
mod runtime_timeout;
use runtime_timeout::runtime_profile_with_timeout_fallback;
#[path = "executor/egress_evidence.rs"]
mod egress_evidence;
use egress_evidence::AgentEgressEvidence;
#[path = "executor/permission_profile.rs"]
mod permission_profile;
use permission_profile::RuntimePermissionProfile;
#[path = "executor/spawn_env.rs"]
mod spawn_env;
use spawn_env::{correction_spawn_env_vars, isolation_spawn_env_vars};
#[path = "executor/structured_output.rs"]
mod structured_output;
use structured_output::{
    codex_output_schema_file, reserve_structured_output_correction_turn,
    structured_output_correction_artifact, structured_output_correction_prompt,
};
pub(super) struct ServerRuntimeJobExecutor<'a> {
    pub(super) state: &'a Arc<AppState>,
    /// Stateful lease-lost signal: `watch` keeps the latest value, so a
    /// cancellation that fires before the turn loop starts polling is not
    /// lost (GH-1877).
    lease_lost: Arc<tokio::sync::watch::Sender<bool>>,
}
impl<'a> ServerRuntimeJobExecutor<'a> {
    pub(super) fn new(state: &'a Arc<AppState>) -> Self {
        let (lease_lost, _) = tokio::sync::watch::channel(false);
        Self {
            state,
            lease_lost: Arc::new(lease_lost),
        }
    }

    pub(super) fn cancel_lease_lost(&self) {
        let _ = self.lease_lost.send(true);
    }

    pub(super) async fn execute_inner(&self, job: RuntimeJob) -> anyhow::Result<ActivityResult> {
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
        let _queue_permit = super::runtime_execution_queue::acquire_runtime_execution_queue_permit(
            self.state,
            workflow.as_ref(),
        )
        .await?;
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
        let activity = activity_name(&job);
        let execution_phase = execution_phase_for_runtime_activity(&activity);
        let resolved_settings = resolve_runtime_settings(
            &runtime_profile,
            job.runtime_kind,
            execution_phase,
            &self.state.core.server.config.agents,
            &self.state.core.server.config.concurrency,
        )?;
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
            let memory_enabled = workflow_document.config.memory.enabled;
            let repo_memory = repo_memory_for_prompt_packet(
                memory_enabled,
                self.state.core.workflow_runtime_store.as_deref(),
                workflow.as_ref(),
                &job,
            )
            .await;
            let prompt_packet = build_runtime_prompt_packet(
                &job,
                workflow.as_ref(),
                &project_root,
                &source_project_root,
                &runtime_profile,
                &resolved_settings,
                &workflow_document,
                &repo_memory.records,
                prompt_task_request.prompt_text(),
            )?;
            let prompt_packet_digest = prompt_packet_digest(&prompt_packet);
            self.record_prompt_packet_prepared(&job, &prompt_packet, &prompt_packet_digest)
                .await?;
            let force_code_agent = force_code_agent_for_runtime_turn(
                job.runtime_kind,
                resolved_settings.approval_policy.explicit_value(),
            );
            let output_schema_file =
                codex_output_schema_file(true, &job, &project_root, &prompt_packet)?;
            let mut prompt =
                build_runtime_job_prompt(&prompt_packet, prompt_task_request.prompt_text());
            let mut correction_retry = None;
            let mut transcript_turn = None;
            let mut attempt_enforcement_artifacts = Vec::new();
            for attempt in 0..=1 {
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
                let _runtime_turn_alias_guard = workflow
                    .as_ref()
                    .and_then(|workflow| {
                        crate::workflow_runtime_submission::runtime_issue_task_handle(workflow)
                    })
                    .map(|submission_id| {
                        RuntimeTurnAliasGuard::register(
                            self.state.core.server.clone(),
                            submission_id.0,
                            turn_id.clone(),
                        )
                    });
                let correction_only = attempt > 0 && correction_retry.is_some();
                let mut env_vars = if correction_only {
                    correction_spawn_env_vars(&job)
                } else {
                    isolation_spawn_env_vars(&job)
                };
                let permission_profile = RuntimePermissionProfile::resolve(
                    resolved_settings.permission_mode,
                    resolved_settings.allowed_tools.clone(),
                    resolved_settings.tool_allowlist_enforcement,
                    correction_only,
                );
                let egress_evidence = AgentEgressEvidence::from_spawn_env(
                    job.runtime_kind,
                    permission_profile.permission_mode,
                    &env_vars,
                );
                let egress_verified_at_dispatch = Arc::new(AtomicBool::new(false));
                if let Some(schema_file) = output_schema_file.as_ref() {
                    env_vars.insert(
                        AGENT_OUTPUT_SCHEMA_PATH_ENV.to_string(),
                        schema_file.argument_path.display().to_string(),
                    );
                }
                run_turn_lifecycle_with_options(
                    self.state.core.server.clone(),
                    self.state.notifications.notify_tx.clone(),
                    self.state.notifications.notification_tx.clone(),
                    thread_id.clone(),
                    turn_id.clone(),
                    prompt.clone(),
                    agent_name.to_string(),
                    TurnLifecycleOptions {
                        model: Some(resolved_settings.model.clone()),
                        reasoning_effort: resolved_settings.reasoning_effort.clone(),
                        execution_phase,
                        sandbox_mode: Some(if correction_only {
                            harness_core::config::agents::SandboxMode::ReadOnly
                        } else {
                            resolved_settings.sandbox_mode
                        }),
                        approval_policy: if correction_only {
                            Some("never".to_string())
                        } else {
                            resolved_settings
                                .approval_policy
                                .explicit_value()
                                .map(str::to_owned)
                        },
                        timeout_secs: Some(resolved_settings.timeout_secs),
                        stall_timeout_secs: Some(resolved_settings.stall_timeout_secs),
                        lease_lost: Some(self.lease_lost.subscribe()),
                        env_vars,
                        permission_mode: permission_profile.permission_mode,
                        allowed_tools: permission_profile.allowed_tools.clone(),
                        force_code_agent: force_code_agent || correction_only,
                        runtime_usage: runtime_usage_context(
                            self.state,
                            &job,
                            workflow.as_ref(),
                            &runtime_profile,
                            agent_name,
                            &source_project_root,
                        ),
                        egress_verified_at_dispatch: Some(Arc::clone(
                            &egress_verified_at_dispatch,
                        )),
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
                let attempt_number = attempt + 1;
                attempt_enforcement_artifacts.push(permission_profile.artifact(
                    resolved_settings.capability_profile,
                    attempt_number,
                ));
                attempt_enforcement_artifacts.push(egress_evidence.artifact(
                    &turn.items,
                    egress_verified_at_dispatch.load(Ordering::Acquire),
                    attempt_number,
                ));
                let mut result = activity_result_from_turn_with_workflow(
                    &job,
                    &turn.status,
                    &turn.items,
                    &thread_id,
                    &turn_id,
                    agent_name,
                    &project_root,
                    &prompt_packet_digest,
                    workflow
                        .as_ref()
                        .map(|workflow| workflow.definition_id.as_str()),
                );
                if attempt == 0 {
                    if let Some(correction) = structured_output_correction(&result) {
                        let retry_budget_available = reserve_structured_output_correction_turn(
                            self.state.core.workflow_runtime_store.as_deref(),
                            &job,
                            resolved_settings.max_turns,
                            attempt + 2,
                        )
                        .await?;
                        if retry_budget_available {
                            tracing::warn!(
                                runtime_job_id = %job.id,
                                activity = %activity,
                                outcome = %correction.outcome,
                                "retrying runtime turn once to correct structured ActivityResult output"
                            );
                            prompt = structured_output_correction_prompt(
                                &prompt,
                                &correction,
                                &turn.items,
                            );
                            correction_retry = Some(correction);
                            transcript_turn = Some(turn.clone());
                            continue;
                        } else {
                            tracing::warn!(
                                runtime_job_id = %job.id,
                                activity = %activity,
                                "skipping structured output correction retry because runtime max_turns is exhausted"
                            );
                        }
                    }
                }
                if let Some(correction) = correction_retry.as_ref() {
                    result = result.with_artifact(structured_output_correction_artifact(
                        correction,
                        attempt + 1,
                    ));
                }
                let result =
                    harness_workflow::runtime::completion_evidence::strip_server_reserved_artifacts(
                        result,
                    );
                let result =
                    super::transcript_durability::attach_runtime_transcript_source(
                        result,
                        transcript_turn.as_ref().unwrap_or(&turn),
                    )?
                    .with_artifact(repo_memory_config_artifact(memory_enabled));
                let result = attempt_enforcement_artifacts
                    .into_iter()
                    .fold(result, |result, artifact| result.with_artifact(artifact));
                let result = if let Some(degradation) = repo_memory.degradation.clone() {
                    result.with_artifact(degradation)
                } else {
                    result
                };
                let result =
                    verify_merge_completion_if_needed(self.state, &job, workflow.as_ref(), result)
                        .await;
                return Ok(
                    super::completion_evidence_integration::apply_completion_evidence(
                        self.state,
                        &job,
                        workflow.as_ref(),
                        &workflow_document.config,
                        &project_root,
                        result,
                    )
                    .await,
                );
            }
            unreachable!("bounded structured-output retry loop always returns")
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
    pub(super) async fn runtime_worker_disabled_result(
        &self,
        job: &RuntimeJob,
    ) -> Option<ActivityResult> {
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
pub(super) fn is_internal_non_agent_activity(job: &RuntimeJob) -> bool {
    is_builtin_lifecycle_activity(job) || is_server_owned_pr_feedback_inspection(job)
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
        "runtime_workspace_finalization_warning",
        json!({ "error": error.to_string() }),
    )
}
#[cfg(test)]
mod tests {
    use super::runtime_timeout::runtime_timeout_fallback;
    use super::*;
    use harness_core::config::workflow::RuntimeDispatchProfileOverride;
    use harness_workflow::runtime::{RuntimeKind, RuntimeProfile, WorkflowSubject};
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
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("finalization failure should be returned as a failed activity result");
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
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
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
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("failed activity result should be preserved");
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
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
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
