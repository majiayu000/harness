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
use sha2::{Digest, Sha256};
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
        if let Some(result) = self
            .execute_builtin_lifecycle_activity(&job, workflow.as_ref())
            .await?
        {
            return Ok(result);
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
        let prompt_packet =
            build_runtime_prompt_packet(&job, workflow.as_ref(), &project_root, &runtime_profile);
        let prompt_packet_digest = prompt_packet_digest(&prompt_packet);
        self.record_prompt_packet_prepared(&job, &prompt_packet, &prompt_packet_digest)
            .await?;
        let prompt = build_runtime_job_prompt(&prompt_packet);
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
            &prompt_packet_digest,
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

    async fn execute_builtin_lifecycle_activity(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<Option<ActivityResult>> {
        match activity_name(job).as_str() {
            "start_child_workflow" => {
                Ok(Some(self.execute_start_child_workflow(job, parent).await?))
            }
            "mark_bound_issue_done" => {
                Ok(Some(self.execute_mark_bound_issue_done(job, parent).await?))
            }
            "recover_issue_workflow" => Ok(Some(
                self.execute_recover_issue_workflow(job, parent).await?,
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

    async fn execute_mark_bound_issue_done(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<ActivityResult> {
        let Some(parent) = parent else {
            anyhow::bail!("mark_bound_issue_done parent workflow is missing");
        };
        let Some(issue_number) = optional_data_u64(parent, "last_issue_number") else {
            return Ok(ActivityResult::succeeded(
                activity_name(job),
                "No bound issue workflow was available to mark done.",
            ));
        };
        let child = self
            .upsert_child_issue_state(
                job,
                parent,
                issue_number,
                "done",
                "BoundIssueMarkedDone",
                json!({
                    "pr_number": optional_data_u64(parent, "last_pr_number"),
                    "pr_url": parent.data.get("last_pr_url").and_then(Value::as_str),
                }),
            )
            .await?;
        Ok(ActivityResult::succeeded(
            activity_name(job),
            format!("Bound issue workflow `{}` marked done.", child.id),
        )
        .with_artifact(ActivityArtifact::new(
            "child_workflow",
            child_workflow_artifact(&child),
        )))
    }

    async fn execute_recover_issue_workflow(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
    ) -> anyhow::Result<ActivityResult> {
        let Some(parent) = parent else {
            anyhow::bail!("recover_issue_workflow parent workflow is missing");
        };
        let Some(issue_number) = optional_data_u64(parent, "last_issue_number") else {
            return Ok(ActivityResult::succeeded(
                activity_name(job),
                "No stale issue workflow was available to recover.",
            ));
        };
        let child = self
            .upsert_child_issue_state(
                job,
                parent,
                issue_number,
                "scheduled",
                "IssueWorkflowRecovered",
                json!({
                    "previous_state": parent.data.get("last_observed_state").and_then(Value::as_str),
                    "previous_active_task_id": parent.data.get("last_active_task_id").and_then(Value::as_str),
                    "recovery_reason": parent.data.get("last_recovery_reason").and_then(Value::as_str),
                }),
            )
            .await?;
        Ok(ActivityResult::succeeded(
            activity_name(job),
            format!("Issue workflow `{}` recovered to scheduled.", child.id),
        )
        .with_artifact(ActivityArtifact::new(
            "child_workflow",
            child_workflow_artifact(&child),
        )))
    }

    async fn upsert_child_issue_state(
        &self,
        job: &RuntimeJob,
        parent: &WorkflowInstance,
        issue_number: u64,
        state: &str,
        event_type: &str,
        update: Value,
    ) -> anyhow::Result<WorkflowInstance> {
        let Some(store) = self.state.core.workflow_runtime_store.as_ref() else {
            anyhow::bail!("workflow runtime store is unavailable");
        };
        let project_id = required_data_string(parent, "project_id")?;
        let repo = parent.data.get("repo").and_then(Value::as_str);
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
                state,
                WorkflowSubject::new("issue", format!("issue:{issue_number}")),
            )
            .with_id(child_id.clone()),
        };
        child.state = state.to_string();
        child.version = child.version.saturating_add(1);
        if child.parent_workflow_id.is_none() {
            child.parent_workflow_id = Some(parent.id.clone());
        }
        child.data = merge_child_issue_data(
            child.data,
            project_id,
            repo,
            issue_number,
            job.id.as_str(),
            job.command_id.as_str(),
        );
        merge_json_object(&mut child.data, update);
        store.upsert_instance(&child).await?;
        store
            .append_event(
                &child.id,
                event_type,
                "workflow_runtime_worker",
                json!({
                    "parent_workflow_id": parent.id.as_str(),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "state": child.state,
                    "issue_number": issue_number,
                }),
            )
            .await?;
        Ok(child)
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

fn build_runtime_prompt_packet(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    project_root: &Path,
    runtime_profile: &RuntimeProfile,
) -> Value {
    json!({
        "schema": "harness.runtime.prompt_packet.v1",
        "runtime_job": {
            "id": job.id,
            "command_id": job.command_id,
            "runtime_kind": job.runtime_kind,
            "runtime_profile": job.runtime_profile,
            "activity": activity_name(job),
        },
        "runtime_profile": runtime_profile,
        "project": {
            "root": project_root.display().to_string(),
            "repo": workflow
                .and_then(|workflow| workflow.data.get("repo"))
                .and_then(Value::as_str)
                .or_else(|| job.input.get("repo").and_then(Value::as_str)),
        },
        "workflow": workflow.map(|workflow| {
            json!({
                "id": workflow.id,
                "definition_id": workflow.definition_id,
                "definition_version": workflow.definition_version,
                "state": workflow.state,
                "version": workflow.version,
                "subject": workflow.subject,
                "parent_workflow_id": workflow.parent_workflow_id,
                "data": workflow.data,
            })
        }),
        "command_input": job.input,
        "runtime_contract": {
            "orchestration_source": "workflow_database",
            "agent_must_not_edit_workflow_tables": true,
            "agent_executes_repository_and_github_work": true,
            "follow_project_instructions": true,
        },
        "activity_result_schema": activity_result_schema(job, workflow),
        "required_structured_output": {
            "summary": "Concise final activity summary.",
            "changed_files": "Files changed by this runtime activity, if any.",
            "validation_commands": "Validation commands run and their results.",
            "remaining_blockers": "Any blockers that still require follow-up.",
        },
    })
}

fn build_runtime_job_prompt(prompt_packet: &Value) -> String {
    let prompt_packet_json = pretty_json(prompt_packet);
    let activity = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("activity"))
        .and_then(Value::as_str)
        .unwrap_or("workflow_activity");
    let project_root = prompt_packet
        .get("project")
        .and_then(|project| project.get("root"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let runtime_profile = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("runtime_profile"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let job_id = prompt_packet
        .get("runtime_job")
        .and_then(|runtime_job| runtime_job.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "You are executing a Harness workflow runtime job.\n\n\
         Runtime contract:\n\
         - Treat the workflow database as the source of orchestration state, but do not edit workflow tables directly.\n\
         - Harness server only manages lifecycle. You, the agent, perform repository and GitHub work when the activity requires it.\n\
         - Follow the project instructions loaded by the runtime.\n\
         - Use the prompt packet activity_result_schema to shape your final summary; Harness will wrap your turn into an ActivityResult.\n\
         - Return a concise final summary with changed files, validation commands, and remaining blockers.\n\n\
         Project root: {project_root}\n\
         Runtime job id: {job_id}\n\
         Runtime profile: {runtime_profile}\n\
         Activity: {activity}\n\n\
         Prompt packet:\n{prompt_packet_json}\n",
    )
}

fn activity_result_schema(job: &RuntimeJob, workflow: Option<&WorkflowInstance>) -> Value {
    let activity = activity_name(job);
    let workflow_definition = workflow
        .map(|workflow| workflow.definition_id.as_str())
        .unwrap_or("unknown");
    let transition_contract = activity_transition_contract(workflow_definition, &activity);
    let summary_contract = agent_summary_contract(workflow_definition, &activity);
    json!({
        "schema": "harness.runtime.activity_result.v1",
        "activity": activity,
        "workflow_definition": workflow_definition,
        "result_type": "ActivityResult",
        "required_fields": ["activity", "status", "summary"],
        "optional_fields": ["artifacts", "signals", "validation", "error", "error_kind"],
        "allowed_statuses": ["succeeded", "failed", "blocked", "cancelled"],
        "allowed_error_kinds": ["retryable", "fatal", "configuration", "external_dependency", "unknown"],
        "status_contract": {
            "succeeded": "The activity completed and its output is ready for the workflow reducer.",
            "failed": "The activity hit an execution error. Use error_kind=fatal or configuration when retry would not help.",
            "blocked": "The activity cannot proceed without external input or budget.",
            "cancelled": "The activity was intentionally stopped.",
        },
        "transition_contract": transition_contract,
        "agent_summary_contract": summary_contract,
    })
}

fn activity_transition_contract(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("github_issue_pr", "replan_issue") => json!({
            "on_succeeded": {
                "reducer_next_state": "implementing",
                "required_summary": "Explain the revised implementation direction and validation plan."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "on_succeeded": {
                "reducer_next_state": "awaiting_feedback",
                "required_summary": "Describe addressed review feedback and validation evidence."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("github_issue_pr", "implement_issue") => json!({
            "on_succeeded": {
                "reducer_next_state": "unchanged_until_pr_detected",
                "required_summary": "Include changed files, validation commands, and the PR URL when one is created."
            },
            "follow_up_event": "PrDetected binds PR metadata and advances the issue workflow."
        }),
        ("repo_backlog", "start_child_workflow") => json!({
            "on_succeeded": {
                "reducer_next_state": "idle",
                "required_artifact": "child_workflow"
            }
        }),
        ("repo_backlog", "mark_bound_issue_done") | ("repo_backlog", "recover_issue_workflow") => {
            json!({
                "on_succeeded": {
                    "reducer_next_state": "idle",
                    "required_artifact": "child_workflow"
                }
            })
        }
        _ => json!({
            "on_succeeded": {
                "reducer_next_state": "unchanged",
                "reason": "No reducer transition is registered for this workflow/activity pair."
            }
        }),
    }
}

fn agent_summary_contract(workflow_definition: &str, activity: &str) -> Value {
    match (workflow_definition, activity) {
        ("github_issue_pr", "implement_issue") => json!({
            "must_include": ["changed files", "validation commands", "PR URL or blocker"],
            "must_not_include": ["workflow table mutations", "unverified merge claims"],
            "artifacts": {
                "pull_request": {
                    "required_when": "A PR was created or reused by the activity.",
                    "fields": ["pr_number", "pr_url"]
                }
            }
        }),
        ("github_issue_pr", "replan_issue") => json!({
            "must_include": ["reason for replan", "new implementation plan", "validation plan"],
            "must_not_include": ["direct workflow state changes"],
        }),
        ("github_issue_pr", "address_pr_feedback") => json!({
            "must_include": ["review feedback addressed", "changed files", "validation commands"],
            "must_not_include": ["claiming review approval without a fresh review signal"],
        }),
        _ => json!({
            "must_include": ["summary", "validation commands", "remaining blockers"],
            "must_not_include": ["direct workflow table mutations"],
        }),
    }
}

fn prompt_packet_digest(prompt_packet: &Value) -> String {
    let bytes = serde_json::to_vec(prompt_packet).unwrap_or_else(|_| Vec::new());
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn workflow_prompt_artifact(prompt_packet_digest: &str) -> ActivityArtifact {
    ActivityArtifact::new(
        "runtime_prompt_packet",
        json!({
            "digest": prompt_packet_digest,
            "schema": "harness.runtime.prompt_packet.v1",
        }),
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
    prompt_packet_digest: &str,
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
        .with_artifact(workflow_prompt_artifact(prompt_packet_digest))
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

fn required_data_string<'a>(
    workflow: &'a WorkflowInstance,
    field: &str,
) -> anyhow::Result<&'a str> {
    workflow
        .data
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("workflow data `{field}` is missing"))
}

fn optional_data_u64(workflow: &WorkflowInstance, field: &str) -> Option<u64> {
    workflow.data.get(field).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    })
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

fn merge_json_object(target: &mut Value, update: Value) {
    let Some(target_object) = target.as_object_mut() else {
        return;
    };
    let Some(update_object) = update.as_object() else {
        return;
    };
    for (key, value) in update_object {
        target_object.insert(key.clone(), value.clone());
    }
}

fn child_workflow_artifact(child: &WorkflowInstance) -> Value {
    json!({
        "workflow_id": child.id,
        "definition_id": child.definition_id,
        "state": child.state,
        "subject_key": child.subject.subject_key,
    })
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
