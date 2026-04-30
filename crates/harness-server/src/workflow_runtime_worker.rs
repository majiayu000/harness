use crate::http::AppState;
use crate::task_executor::turn_lifecycle::TurnLifecycleOptions;
use anyhow::Context;
use async_trait::async_trait;
use chrono::Duration;
use harness_core::config::agents::SandboxMode;
use harness_core::types::{AgentId, Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivitySignal, RuntimeJob, RuntimeJobExecutor,
    RuntimeJobStatus, RuntimeKind, RuntimeProfile, RuntimeWorker, WorkflowDefinition,
    WorkflowInstance, WorkflowSubject,
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
        if is_start_child_workflow_job(&job) {
            return self
                .execute_start_child_workflow(&job, workflow.as_ref())
                .await;
        }
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
        let sandbox_mode = runtime_profile_sandbox_mode(&runtime_profile)?;
        let approval_policy = runtime_profile_approval_policy(&runtime_profile, job.runtime_kind)?;
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

    async fn execute_start_child_workflow(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<ActivityResult> {
        let Some(store) = self.state.core.workflow_runtime_store.as_ref() else {
            anyhow::bail!("workflow runtime store is unavailable");
        };
        let command = job
            .input
            .get("command")
            .ok_or_else(|| anyhow::anyhow!("start_child_workflow command payload is missing"))?;
        let definition_id = required_string(command, "definition_id")?;
        let subject_key = required_string(command, "subject_key")?;
        if definition_id != "github_issue_pr" {
            anyhow::bail!("start_child_workflow definition `{definition_id}` is not supported yet");
        }
        let issue_number = parse_issue_subject_key(subject_key)?;
        let project_id = parent
            .and_then(|workflow| workflow.data.get("project_id"))
            .and_then(Value::as_str)
            .or_else(|| job.input.get("project_id").and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("start_child_workflow project_id is missing"))?;
        let repo = parent
            .and_then(|workflow| workflow.data.get("repo"))
            .and_then(Value::as_str)
            .or_else(|| command.get("repo").and_then(Value::as_str));
        let child_id =
            harness_workflow::issue_lifecycle::workflow_id(project_id, repo, issue_number);
        store
            .upsert_definition(&WorkflowDefinition::new(
                "github_issue_pr",
                1,
                "GitHub issue PR workflow",
            ))
            .await?;
        let mut child = match store.get_instance(&child_id).await? {
            Some(instance) => instance,
            None => WorkflowInstance::new(
                "github_issue_pr",
                1,
                "discovered",
                WorkflowSubject::new("issue", subject_key),
            )
            .with_id(child_id.clone()),
        };
        if child.parent_workflow_id.is_none() {
            if let Some(parent) = parent {
                child.parent_workflow_id = Some(parent.id.clone());
            }
        }
        child.data = merge_child_issue_data(
            child.data,
            project_id,
            repo,
            issue_number,
            job.id.as_str(),
            job.command_id.as_str(),
        );
        store.upsert_instance(&child).await?;
        store
            .append_event(
                &child.id,
                "ChildWorkflowStarted",
                "workflow_runtime_worker",
                json!({
                    "parent_workflow_id": parent.map(|workflow| workflow.id.as_str()),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "definition_id": definition_id,
                    "subject_key": subject_key,
                }),
            )
            .await?;

        Ok(ActivityResult::succeeded(
            activity_name(job),
            format!("Child workflow `{}` started.", child.id),
        )
        .with_artifact(ActivityArtifact::new(
            "child_workflow",
            json!({
                "workflow_id": child.id,
                "definition_id": child.definition_id,
                "state": child.state,
                "subject_key": child.subject.subject_key,
            }),
        )))
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

fn runtime_profile_sandbox_mode(profile: &RuntimeProfile) -> anyhow::Result<Option<SandboxMode>> {
    let Some(raw) = profile.sandbox.as_deref() else {
        return Ok(None);
    };
    let mode = match raw {
        "read-only" => SandboxMode::ReadOnly,
        "workspace-write" => SandboxMode::WorkspaceWrite,
        "danger-full-access" => SandboxMode::DangerFullAccess,
        other => anyhow::bail!("runtime profile sandbox `{other}` is not supported"),
    };
    Ok(Some(mode))
}

fn runtime_profile_approval_policy(
    profile: &RuntimeProfile,
    runtime_kind: RuntimeKind,
) -> anyhow::Result<Option<String>> {
    let Some(raw) = profile.approval_policy.as_deref() else {
        return Ok(None);
    };
    match raw {
        "untrusted" | "on-failure" | "on-request" | "never" => {}
        other => anyhow::bail!("runtime profile approval_policy `{other}` is not supported"),
    }
    match runtime_kind {
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc => Ok(Some(raw.to_string())),
        other => anyhow::bail!(
            "runtime profile approval_policy `{raw}` is only supported for Codex runtime kinds, not {}",
            runtime_kind_label(other)
        ),
    }
}

fn runtime_kind_label(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::CodexExec => "codex_exec",
        RuntimeKind::CodexJsonrpc => "codex_jsonrpc",
        RuntimeKind::ClaudeCode => "claude_code",
        RuntimeKind::AnthropicApi => "anthropic_api",
        RuntimeKind::RemoteHost => "remote_host",
    }
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

fn is_start_child_workflow_job(job: &RuntimeJob) -> bool {
    job.input.get("command_type").and_then(Value::as_str) == Some("start_child_workflow")
}

fn activity_name(job: &RuntimeJob) -> String {
    job.input
        .get("activity")
        .and_then(Value::as_str)
        .filter(|activity| !activity.trim().is_empty())
        .or_else(|| {
            job.input
                .get("command")
                .and_then(|command| command.get("activity"))
                .and_then(Value::as_str)
                .filter(|activity| !activity.trim().is_empty())
        })
        .or_else(|| {
            job.input
                .get("command_type")
                .and_then(Value::as_str)
                .filter(|command_type| !command_type.trim().is_empty())
        })
        .unwrap_or("workflow_activity")
        .to_string()
}

fn required_string<'a>(value: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("start_child_workflow `{field}` is missing"))
}

fn parse_issue_subject_key(subject_key: &str) -> anyhow::Result<u64> {
    subject_key
        .strip_prefix("issue:")
        .unwrap_or(subject_key)
        .parse::<u64>()
        .with_context(|| format!("start_child_workflow subject_key `{subject_key}` is invalid"))
}

fn merge_child_issue_data(
    mut data: Value,
    project_id: &str,
    repo: Option<&str>,
    issue_number: u64,
    runtime_job_id: &str,
    command_id: &str,
) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    if let Some(object) = data.as_object_mut() {
        object.insert("project_id".to_string(), json!(project_id));
        object.insert("repo".to_string(), json!(repo));
        object.insert("issue_number".to_string(), json!(issue_number));
        object.insert(
            "started_by_runtime_job_id".to_string(),
            json!(runtime_job_id),
        );
        object.insert("started_by_command_id".to_string(), json!(command_id));
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(Path::new(project_id), data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_name_uses_top_level_runtime_activity_key() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "start_child_workflow",
                "command_type": "start_child_workflow",
                "command": {
                    "definition_id": "github_issue_pr",
                    "subject_key": "issue:123"
                }
            }),
        );

        assert_eq!(activity_name(&job), "start_child_workflow");
    }

    #[test]
    fn activity_name_falls_back_to_command_type() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "command_type": "start_child_workflow",
                "command": {
                    "definition_id": "github_issue_pr",
                    "subject_key": "issue:123"
                }
            }),
        );

        assert_eq!(activity_name(&job), "start_child_workflow");
    }
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

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn runtime_profile_approval_policy_accepts_codex_values() {
        for value in ["untrusted", "on-failure", "on-request", "never"] {
            let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
            profile.approval_policy = Some(value.to_string());

            assert_eq!(
                runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
                    .expect("codex approval policy should be accepted"),
                Some(value.to_string())
            );
        }
    }

    #[test]
    fn runtime_profile_approval_policy_rejects_unknown_values() {
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
        profile.approval_policy = Some("always".to_string());

        let error = runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
            .expect_err("unknown approval policy should fail");

        assert!(error
            .to_string()
            .contains("runtime profile approval_policy `always` is not supported"));
    }

    #[test]
    fn runtime_profile_approval_policy_rejects_non_codex_runtimes() {
        let mut profile = RuntimeProfile::new("claude-default", RuntimeKind::ClaudeCode);
        profile.approval_policy = Some("on-request".to_string());

        let error = runtime_profile_approval_policy(&profile, RuntimeKind::ClaudeCode)
            .expect_err("Claude approval policy should fail until it has a contract");

        assert!(error
            .to_string()
            .contains("only supported for Codex runtime kinds"));
    }
}
