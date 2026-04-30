use crate::http::AppState;
use crate::task_executor::turn_lifecycle::TurnLifecycleOptions;
use anyhow::Context;
use async_trait::async_trait;
use chrono::Duration;
use harness_core::types::{AgentId, Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivitySignal, RuntimeJob, RuntimeJobExecutor,
    RuntimeJobStatus, RuntimeKind, RuntimeProfile, RuntimeWorker, WorkflowInstance,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeJobWorkerTick {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub idle: bool,
}

impl RuntimeJobWorkerTick {
    fn from_completed_job(job: Option<RuntimeJob>) -> Self {
        match job.map(|job| job.status) {
            Some(RuntimeJobStatus::Succeeded) => Self {
                succeeded: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Failed) => Self {
                failed: 1,
                ..Self::default()
            },
            Some(RuntimeJobStatus::Cancelled) => Self {
                cancelled: 1,
                ..Self::default()
            },
            Some(
                RuntimeJobStatus::Pending | RuntimeJobStatus::Running | RuntimeJobStatus::Expired,
            ) => Self::default(),
            None => Self {
                idle: true,
                ..Self::default()
            },
        }
    }

    pub(crate) fn touched_anything(&self) -> bool {
        self.succeeded > 0 || self.failed > 0 || self.cancelled > 0
    }
}

pub(crate) async fn run_runtime_job_worker_tick(
    state: &Arc<AppState>,
    owner: impl Into<String>,
    lease_ttl: Duration,
) -> anyhow::Result<RuntimeJobWorkerTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeJobWorkerTick {
            idle: true,
            ..RuntimeJobWorkerTick::default()
        });
    };
    let worker = RuntimeWorker::new(store.as_ref(), owner).with_lease_ttl(lease_ttl);
    let executor = ServerRuntimeJobExecutor::new(state.clone());
    let completed = worker.run_once(&executor).await?;
    Ok(RuntimeJobWorkerTick::from_completed_job(completed))
}

struct ServerRuntimeJobExecutor {
    state: Arc<AppState>,
}

impl ServerRuntimeJobExecutor {
    fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    async fn execute_inner(&self, job: RuntimeJob) -> anyhow::Result<ActivityResult> {
        let workflow = self.workflow_for_job(&job).await?;
        let project_root = self.project_root_for_job(&job, workflow.as_ref())?;
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

        let runtime_profile = runtime_profile_for_job(&job)?;
        let prompt =
            build_runtime_job_prompt(&job, workflow.as_ref(), &project_root, &runtime_profile);
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
        persist_created_thread(&self.state, &thread_id).await;

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
        ))
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

    fn project_root_for_job(
        &self,
        job: &RuntimeJob,
        workflow: Option<&WorkflowInstance>,
    ) -> anyhow::Result<PathBuf> {
        if let Some(project_id) = workflow
            .and_then(|workflow| workflow.data.get("project_id"))
            .and_then(Value::as_str)
            .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        {
            let project_root = PathBuf::from(project_id);
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
impl RuntimeJobExecutor for ServerRuntimeJobExecutor {
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

fn agent_name_for_runtime_kind(kind: RuntimeKind) -> anyhow::Result<&'static str> {
    match kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => Ok("codex"),
        RuntimeKind::ClaudeCode => Ok("claude"),
        RuntimeKind::AnthropicApi => Ok("anthropic-api"),
        RuntimeKind::RemoteHost => {
            anyhow::bail!("remote_host runtime jobs must be claimed by an external runtime host")
        }
    }
}

fn build_runtime_job_prompt(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    project_root: &Path,
    runtime_profile: &RuntimeProfile,
) -> String {
    let workflow_json = workflow
        .map(pretty_json)
        .unwrap_or_else(|| "null".to_string());
    let command_json = pretty_json(&job.input);
    let profile_json = pretty_json(runtime_profile);
    format!(
        "You are executing a Harness workflow runtime job.\n\n\
         Runtime contract:\n\
         - Treat the workflow database as the source of orchestration state, but do not edit workflow tables directly.\n\
         - Harness server only manages lifecycle. You, the agent, perform repository and GitHub work when the activity requires it.\n\
         - Follow the project instructions loaded by the runtime.\n\
         - Return a concise final summary with changed files, validation commands, and remaining blockers.\n\n\
         Project root: {project_root}\n\
         Runtime job id: {job_id}\n\
         Runtime profile: {runtime_profile}\n\
         Activity: {activity}\n\n\
         Runtime profile metadata:\n{profile_json}\n\n\
         Workflow instance:\n{workflow_json}\n\n\
         Runtime command input:\n{command_json}\n",
        project_root = project_root.display(),
        job_id = job.id.as_str(),
        runtime_profile = job.runtime_profile.as_str(),
        activity = activity_name(job),
    )
}

fn runtime_profile_for_job(job: &RuntimeJob) -> anyhow::Result<RuntimeProfile> {
    let Some(value) = job.input.get("runtime_profile") else {
        return Ok(RuntimeProfile::new(
            job.runtime_profile.clone(),
            job.runtime_kind,
        ));
    };
    serde_json::from_value(value.clone())
        .with_context(|| format!("runtime job {} has invalid runtime_profile input", job.id))
}

fn activity_result_from_turn(
    job: &RuntimeJob,
    status: &TurnStatus,
    items: &[Item],
    thread_id: &ThreadId,
    turn_id: &TurnId,
    agent_name: &str,
    project_root: &Path,
) -> ActivityResult {
    let activity = activity_name(job);
    let summary = last_agent_summary(items).unwrap_or_else(|| match status {
        TurnStatus::Completed => "Agent turn completed.".to_string(),
        TurnStatus::Cancelled => "Agent turn was cancelled.".to_string(),
        TurnStatus::Failed => "Agent turn failed.".to_string(),
        TurnStatus::Running => "Agent turn is still running after lifecycle returned.".to_string(),
    });
    let result = match status {
        TurnStatus::Completed => ActivityResult::succeeded(activity, summary),
        TurnStatus::Cancelled => ActivityResult::cancelled(activity, summary),
        TurnStatus::Failed | TurnStatus::Running => ActivityResult::failed(
            activity,
            summary,
            last_error(items).unwrap_or_else(|| "agent turn failed".to_string()),
        ),
    };
    result
        .with_artifact(ActivityArtifact::new(
            "runtime_turn",
            json!({
                "thread_id": thread_id.as_str(),
                "turn_id": turn_id.as_str(),
                "agent": agent_name,
                "project_root": project_root.display().to_string(),
            }),
        ))
        .with_signal(ActivitySignal::new(
            "RuntimeTurnCompleted",
            json!({
                "status": status,
                "runtime_job_id": job.id.as_str(),
            }),
        ))
}

fn activity_name(job: &RuntimeJob) -> String {
    job.input
        .get("command")
        .and_then(|command| command.get("activity"))
        .and_then(Value::as_str)
        .unwrap_or("workflow_activity")
        .to_string()
}

fn last_agent_summary(items: &[Item]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        Item::AgentReasoning { content } if !content.trim().is_empty() => {
            Some(truncate_summary(content.trim()))
        }
        _ => None,
    })
}

fn last_error(items: &[Item]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        Item::Error { message, .. } if !message.trim().is_empty() => {
            Some(truncate_summary(message.trim()))
        }
        _ => None,
    })
}

fn truncate_summary(value: &str) -> String {
    const LIMIT: usize = 1200;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    let mut boundary = LIMIT;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}...", &value[..boundary])
}

fn pretty_json<T>(value: &T) -> String
where
    T: serde::Serialize,
{
    serde_json::to_string_pretty(value).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string()
        })
        .to_string()
    })
}
