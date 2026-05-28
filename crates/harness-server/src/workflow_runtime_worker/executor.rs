use crate::http::AppState;
use crate::task_executor::turn_lifecycle::TurnLifecycleOptions;
use async_trait::async_trait;
use harness_core::config::workflow::{RuntimeDispatchProfileOverride, WorkflowConfig};
use harness_core::types::{AgentId, ThreadId};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, RuntimeJob, RuntimeJobExecutor, RuntimeProfile,
    WorkflowInstance,
};
use serde_json::{json, Value};
use std::sync::Arc;

use super::activity_result::activity_result_from_turn;
use super::child_workflow::{
    execute_mark_bound_issue_done, execute_recover_issue_workflow, execute_start_child_workflow,
};
use super::data_helpers::{
    activity_name, is_builtin_lifecycle_activity, prompt_payload_unavailable_result,
    prompt_task_request_for_job, PromptTaskRequest,
};
use super::prompt_packet::{
    build_runtime_job_prompt, build_runtime_prompt_packet, prompt_packet_digest,
};
use super::runtime_profile::{
    agent_name_for_runtime_kind, runtime_profile_approval_policy, runtime_profile_for_job,
    runtime_profile_sandbox_mode,
};
use super::workspace::{finish_runtime_workspace, prepare_runtime_workspace};

const DEFAULT_RUNTIME_TURN_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_PR_FEEDBACK_INSPECT_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_REPO_BACKLOG_POLL_TIMEOUT_SECS: u64 = 3600;

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
            .execute_builtin_lifecycle_activity(&job, workflow.as_ref())
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
        let prompt_task_request = prompt_task_request_for_job(&job)?;
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

            crate::task_executor::turn_lifecycle::run_turn_lifecycle_with_options(
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
                    sandbox_mode,
                    approval_policy,
                    timeout_secs: runtime_profile.timeout_secs,
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
            Ok(activity_result_from_turn(
                &job,
                &turn.status,
                &turn.items,
                &thread_id,
                &turn_id,
                agent_name,
                &project_root,
                &prompt_packet_digest,
            ))
        }
        .await;
        let finish_result = finish_runtime_workspace(self.state, &runtime_workspace).await;
        match (activity_result, finish_result) {
            (Ok(mut result), Err(error)) => {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %runtime_workspace.run_project.display(),
                    "runtime workspace finalization failed: {error}"
                );
                result = result.with_artifact(ActivityArtifact::new(
                    "runtime_workspace_finalization_warning",
                    json!({ "error": error.to_string() }),
                ));
                Ok(result)
            }
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), Err(finish_error)) => {
                tracing::warn!(
                    runtime_job_id = %job.id,
                    workspace_path = %runtime_workspace.run_project.display(),
                    "runtime workspace finalization failed after runtime error: {finish_error}"
                );
                Err(error.context(format!(
                    "runtime workspace finalization also failed: {finish_error}"
                )))
            }
            (Err(error), Ok(())) => Err(error),
        }
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

    async fn execute_builtin_lifecycle_activity(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<Option<ActivityResult>> {
        match activity_name(job).as_str() {
            "start_child_workflow" => Ok(Some(
                execute_start_child_workflow(self.state, job, parent).await?,
            )),
            "mark_bound_issue_done" => Ok(Some(
                execute_mark_bound_issue_done(self.state, job, parent).await?,
            )),
            "recover_issue_workflow" => Ok(Some(
                execute_recover_issue_workflow(self.state, job, parent).await?,
            )),
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
}

#[async_trait]
impl RuntimeJobExecutor for ServerRuntimeJobExecutor<'_> {
    fn consumes_runtime_turn(&self, job: &RuntimeJob) -> bool {
        !is_builtin_lifecycle_activity(job)
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
        "poll_repo_backlog" => DEFAULT_REPO_BACKLOG_POLL_TIMEOUT_SECS,
        _ => DEFAULT_RUNTIME_TURN_TIMEOUT_SECS,
    })
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
            runtime_timeout_fallback(&config, None, &runtime_job("poll_repo_backlog")),
            Some(3600)
        );
        assert_eq!(
            runtime_timeout_fallback(&config, None, &runtime_job("implement_issue")),
            Some(3600)
        );
    }
}
