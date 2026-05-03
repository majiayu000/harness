use crate::http::AppState;
use crate::task_executor::turn_lifecycle::TurnLifecycleOptions;
use crate::task_runner::{TaskFailureKind, TaskId, TaskKind, TaskState, TaskStatus};
use anyhow::Context;
use async_trait::async_trait;
use chrono::Duration;
use harness_core::config::agents::SandboxMode;
use harness_core::types::{AgentId, Item, ThreadId, TurnId, TurnStatus};
use harness_workflow::runtime::{
    build_pr_feedback_inspect_decision, ActivityArtifact, ActivityErrorKind, ActivityResult,
    ActivitySignal, DecisionValidator, PrFeedbackInspectDecisionInput, RuntimeJob,
    RuntimeJobExecutor, RuntimeJobStatus, RuntimeKind, RuntimeProfile, RuntimeWorker,
    WorkflowCommandRecord, WorkflowDecisionRecord, WorkflowDefinition, WorkflowInstance,
    WorkflowSubject, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
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
    if let Some(job) = completed.as_ref() {
        if let Err(error) = notify_runtime_issue_terminal(state, job).await {
            tracing::warn!(
                runtime_job_id = %job.id,
                "workflow runtime issue completion notification failed: {error}"
            );
        }
    }
    Ok(RuntimeJobWorkerTick::from_completed_job(completed))
}

pub(crate) async fn notify_runtime_issue_terminal(
    state: &AppState,
    job: &RuntimeJob,
) -> anyhow::Result<bool> {
    let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let result = runtime_job_activity_result(job);
    notify_runtime_issue_terminal_workflow(state, workflow_id, result.as_ref()).await
}

pub(crate) async fn notify_runtime_issue_terminal_workflow(
    state: &AppState,
    workflow_id: &str,
    result: Option<&ActivityResult>,
) -> anyhow::Result<bool> {
    let Some(callback) = state.intake.completion_callback.as_ref() else {
        return Ok(false);
    };
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(false);
    };
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(false);
    };
    let Some(task) = runtime_issue_completion_task(&instance, result) else {
        return Ok(false);
    };
    callback(task).await;
    Ok(true)
}

fn runtime_issue_completion_task(
    instance: &WorkflowInstance,
    result: Option<&ActivityResult>,
) -> Option<TaskState> {
    if instance.definition_id != "github_issue_pr" || !instance.is_terminal() {
        return None;
    }
    let source = optional_data_string(instance, "source")?;
    let external_id = optional_data_string(instance, "external_id")?;
    let task_id = crate::workflow_runtime_submission::runtime_issue_task_handle(instance)?;
    let status = task_status_for_runtime_state(&instance.state)?;
    let mut task = TaskState::new(task_id);
    task.task_kind = TaskKind::Issue;
    task.status = status.clone();
    task.failure_kind = runtime_failure_kind(&status, result);
    task.source = Some(source);
    task.external_id = Some(external_id);
    task.repo = optional_data_string(instance, "repo");
    task.issue = optional_data_u64(instance, "issue_number");
    task.project_root = optional_data_string(instance, "project_id").map(PathBuf::from);
    task.pr_url = optional_data_string(instance, "last_pr_url");
    task.description = task.issue.map(|issue| format!("issue #{issue}"));
    task.error = runtime_completion_error(&status, result);
    Some(task)
}

fn task_status_for_runtime_state(state: &str) -> Option<TaskStatus> {
    match state {
        "done" => Some(TaskStatus::Done),
        "failed" => Some(TaskStatus::Failed),
        "cancelled" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

fn runtime_job_activity_result(job: &RuntimeJob) -> Option<ActivityResult> {
    job.output
        .as_ref()
        .and_then(|output| serde_json::from_value(output.clone()).ok())
}

fn runtime_failure_kind(
    status: &TaskStatus,
    result: Option<&ActivityResult>,
) -> Option<TaskFailureKind> {
    if !status.is_failure() {
        return None;
    }
    match result.and_then(|result| result.error_kind) {
        Some(ActivityErrorKind::Retryable | ActivityErrorKind::ExternalDependency) => {
            Some(TaskFailureKind::WorkspaceLifecycle)
        }
        _ => Some(TaskFailureKind::Task),
    }
}

fn runtime_completion_error(
    status: &TaskStatus,
    result: Option<&ActivityResult>,
) -> Option<String> {
    if !status.is_failure() {
        return None;
    }
    result
        .and_then(|result| {
            result
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| (!result.summary.trim().is_empty()).then_some(result.summary.trim()))
        })
        .map(ToOwned::to_owned)
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
        if definition_id == PR_FEEDBACK_DEFINITION_ID {
            return self
                .execute_start_pr_feedback_child_workflow(job, parent, command, subject_key)
                .await;
        }
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

        let mut child_submission = None;
        if command
            .get("auto_submit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let labels = string_vec(command, "labels");
            let force_execute = force_execute_from_project_policy(project_id, &labels);
            let task_id = TaskId::from_str(&format!(
                "repo-backlog:{}:issue:{issue_number}",
                repo.unwrap_or("<none>")
            ));
            let source = optional_string(command, "source").unwrap_or_else(|| "github".to_string());
            let external_id =
                optional_string(command, "external_id").unwrap_or_else(|| issue_number.to_string());
            let depends_on = dependency_task_ids_from_command(command, repo);
            let submission = crate::workflow_runtime_submission::record_issue_submission(
                store,
                crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                    project_root: Path::new(project_id),
                    repo,
                    issue_number,
                    task_id: &task_id,
                    labels: &labels,
                    force_execute,
                    additional_prompt: None,
                    depends_on: &depends_on,
                    dependencies_blocked: !depends_on.is_empty(),
                    source: Some(source.as_str()),
                    external_id: Some(external_id.as_str()),
                },
            )
            .await?;
            child_submission = Some(submission);
            if let Some(updated) = store.get_instance(&child.id).await? {
                child = updated;
            }
        }

        let mut result = ActivityResult::succeeded(
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
        ));
        if let Some(submission) = child_submission {
            result = result.with_artifact(ActivityArtifact::new(
                "child_submission",
                json!({
                    "workflow_id": submission.workflow_id,
                    "accepted": submission.accepted,
                    "decision_id": submission.decision_id,
                    "command_ids": submission.command_ids,
                    "rejection_reason": submission.rejection_reason,
                }),
            ));
        }
        Ok(result)
    }

    async fn execute_start_pr_feedback_child_workflow(
        &self,
        job: &RuntimeJob,
        parent: Option<&WorkflowInstance>,
        command: &Value,
        subject_key: &str,
    ) -> anyhow::Result<ActivityResult> {
        let Some(store) = self.state.core.workflow_runtime_store.as_ref() else {
            anyhow::bail!("workflow runtime store is unavailable");
        };
        let parent = parent
            .ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow requires a parent"))?;
        let pr_number = parse_pr_subject_key(subject_key)
            .or_else(|| command.get("pr_number").and_then(Value::as_u64))
            .ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow pr_number is missing"))?;
        let project_id = parent
            .data
            .get("project_id")
            .and_then(Value::as_str)
            .or_else(|| job.input.get("project_id").and_then(Value::as_str))
            .ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow project_id is missing"))?;
        let repo = command
            .get("repo")
            .and_then(Value::as_str)
            .or_else(|| parent.data.get("repo").and_then(Value::as_str));
        let pr_url = command
            .get("pr_url")
            .and_then(Value::as_str)
            .or_else(|| parent.data.get("pr_url").and_then(Value::as_str));
        let issue_number = command
            .get("issue_number")
            .and_then(Value::as_u64)
            .or_else(|| parent.data.get("issue_number").and_then(Value::as_u64));
        let child_id = format!("{}::pr-feedback:{}", parent.id, job.command_id);
        store
            .upsert_definition(&WorkflowDefinition::new(
                PR_FEEDBACK_DEFINITION_ID,
                1,
                "PR feedback workflow",
            ))
            .await?;
        let mut child = match store.get_instance(&child_id).await? {
            Some(instance) => instance,
            None => WorkflowInstance::new(
                PR_FEEDBACK_DEFINITION_ID,
                1,
                "pending",
                WorkflowSubject::new("pr", subject_key),
            )
            .with_id(child_id.clone()),
        };
        if child.parent_workflow_id.is_none() {
            child.parent_workflow_id = Some(parent.id.clone());
        }
        child.data = merge_pr_feedback_child_data(
            child.data,
            PrFeedbackChildData {
                project_id,
                repo,
                issue_number,
                pr_number,
                pr_url,
                parent_workflow_id: parent.id.as_str(),
                runtime_job_id: job.id.as_str(),
                command_id: job.command_id.as_str(),
            },
        );
        store.upsert_instance(&child).await?;
        store
            .append_event(
                &child.id,
                "ChildWorkflowStarted",
                "workflow_runtime_worker",
                json!({
                    "parent_workflow_id": parent.id.as_str(),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "definition_id": PR_FEEDBACK_DEFINITION_ID,
                    "subject_key": subject_key,
                }),
            )
            .await?;

        let child_command_ids = if child.state == "pending" {
            let event = store
                .append_event(
                    &child.id,
                    "PrFeedbackInspectionRequested",
                    "workflow_runtime_worker",
                    json!({
                        "parent_workflow_id": parent.id.as_str(),
                        "pr_number": pr_number,
                        "pr_url": pr_url,
                        "issue_number": issue_number,
                        "repo": repo,
                    }),
                )
                .await?;
            let stable_inspect_dedupe_key = format!("pr-feedback-child:{}:inspect", child.id);
            let existing_child_commands = store.commands_for(&child.id).await?;
            let active_inspect_command = existing_child_commands
                .iter()
                .find(|record| is_active_pr_feedback_inspect_command(record));
            let inspect_dedupe_key = match active_inspect_command {
                Some(record) => record.command.dedupe_key.clone(),
                None if existing_child_commands
                    .iter()
                    .any(is_pr_feedback_inspect_command) =>
                {
                    format!("{}:retry:{}", stable_inspect_dedupe_key, event.id)
                }
                None => stable_inspect_dedupe_key,
            };
            let output = build_pr_feedback_inspect_decision(
                &child,
                PrFeedbackInspectDecisionInput {
                    dedupe_key: &inspect_dedupe_key,
                    pr_number,
                    pr_url,
                    issue_number,
                    repo,
                    parent_workflow_id: Some(parent.id.as_str()),
                    summary: "PR feedback child workflow requested runtime inspection.",
                },
            );
            let validation = DecisionValidator::pr_feedback().validate(
                &child,
                &output.decision,
                &harness_workflow::runtime::ValidationContext::new(
                    "workflow_runtime_worker",
                    chrono::Utc::now(),
                ),
            );
            let record = match validation {
                Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event.id)),
                Err(error) => {
                    let record = WorkflowDecisionRecord::rejected(
                        output.decision,
                        Some(event.id),
                        error.to_string(),
                    );
                    store.record_decision(&record).await?;
                    return Ok(ActivityResult::failed(
                        activity_name(job),
                        "PR feedback child workflow inspection request was rejected.",
                        error.to_string(),
                    )
                    .with_error_kind(ActivityErrorKind::Configuration));
                }
            };
            store.record_decision(&record).await?;
            let mut command_ids = Vec::new();
            for command in &output.decision.commands {
                let command_id = store
                    .enqueue_command(&child.id, Some(&record.id), command)
                    .await?;
                let command_record = store
                    .get_command(&command_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("workflow command `{command_id}` not found"))?;
                if !is_active_pr_feedback_inspect_command(&command_record) {
                    anyhow::bail!(
                        "pr_feedback child inspect command `{command_id}` was not queued for dispatch"
                    );
                }
                command_ids.push(command_id);
            }
            child.state = output.decision.next_state;
            child.version = child.version.saturating_add(1);
            store.upsert_instance(&child).await?;
            command_ids
        } else {
            Vec::new()
        };

        Ok(ActivityResult::succeeded(
            activity_name(job),
            format!("PR feedback child workflow `{}` started.", child.id),
        )
        .with_artifact(ActivityArtifact::new(
            "child_workflow",
            json!({
                "workflow_id": child.id,
                "definition_id": child.definition_id,
                "state": child.state,
                "subject_key": child.subject.subject_key,
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            "child_commands",
            json!({
                "command_ids": child_command_ids,
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

fn is_active_pr_feedback_inspect_command(record: &WorkflowCommandRecord) -> bool {
    matches!(record.status.as_str(), "pending" | "dispatched")
        && is_pr_feedback_inspect_command(record)
}

fn is_pr_feedback_inspect_command(record: &WorkflowCommandRecord) -> bool {
    record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
}

#[async_trait]
impl RuntimeJobExecutor for ServerRuntimeJobExecutor {
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
         - Use the prompt packet activity_result_schema to shape your final summary.\n\
         - When returning structured activity output, put a JSON object in a final fenced `harness-activity-result` block matching activity_result_schema.\n\
         - The structured result activity field must match this runtime job activity exactly.\n\
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
    let decision_contract = workflow_decision_contract(workflow);
    json!({
        "schema": "harness.runtime.activity_result.v1",
        "activity": activity,
        "workflow_definition": workflow_definition,
        "result_type": "ActivityResult",
        "required_fields": ["activity", "status", "summary"],
        "optional_fields": ["artifacts", "signals", "validation", "error", "error_kind"],
        "allowed_statuses": ["succeeded", "failed", "blocked", "cancelled"],
        "allowed_error_kinds": ["retryable", "fatal", "configuration", "external_dependency", "unknown"],
        "optional_artifacts": {
            "workflow_decision": {
                "description": "A proposed WorkflowDecision. Harness validates it before applying any transition or command.",
                "required_fields": ["workflow_id", "observed_state", "decision", "next_state", "reason", "confidence"],
                "allowed_confidence": ["low", "medium", "high"]
            }
        },
        "status_contract": {
            "succeeded": "The activity completed and its output is ready for the workflow reducer.",
            "failed": "The activity hit an execution error. Use error_kind=fatal or configuration when retry would not help.",
            "blocked": "The activity cannot proceed without external input or budget.",
            "cancelled": "The activity was intentionally stopped.",
        },
        "transition_contract": transition_contract,
        "workflow_decision_contract": decision_contract,
        "agent_summary_contract": summary_contract,
    })
}

fn workflow_decision_contract(workflow: Option<&WorkflowInstance>) -> Value {
    let Some(workflow) = workflow else {
        return json!({
            "available": false,
            "reason": "No workflow instance was loaded for this runtime job."
        });
    };
    let Some(validator) = decision_validator_for_definition(&workflow.definition_id) else {
        return json!({
            "available": false,
            "workflow_id": workflow.id.as_str(),
            "workflow_definition": workflow.definition_id.as_str(),
            "observed_state": workflow.state.as_str(),
            "reason": "No transition validator is registered for this workflow definition."
        });
    };
    let allowed_transitions = validator
        .transition_rules_from(&workflow.state)
        .map(|rule| {
            let allowed_commands = rule
                .allowed_commands
                .iter()
                .map(|command| command.as_str())
                .collect::<Vec<_>>();
            json!({
                "from_state": rule.from_state.as_deref().unwrap_or("*"),
                "next_state": rule.to_state.as_str(),
                "allowed_commands": allowed_commands,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "available": true,
        "workflow_id": workflow.id.as_str(),
        "workflow_definition": workflow.definition_id.as_str(),
        "observed_state": workflow.state.as_str(),
        "allowed_transitions": allowed_transitions,
    })
}

fn decision_validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    match definition_id {
        "github_issue_pr" => Some(DecisionValidator::github_issue_pr()),
        QUALITY_GATE_DEFINITION_ID => Some(DecisionValidator::quality_gate()),
        PR_FEEDBACK_DEFINITION_ID => Some(DecisionValidator::pr_feedback()),
        "repo_backlog" => Some(DecisionValidator::repo_backlog()),
        _ => None,
    }
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
        ("github_issue_pr", "sweep_pr_feedback")
        | ("github_issue_pr", PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "derived_from_structured_decision_or_signals",
                "accepted_signals": ["FeedbackFound", "NoFeedbackFound", "PrReadyToMerge", "ChangesRequested", "ChecksFailed"],
                "required_summary": "Describe inspected PR feedback, review state, checks, and mergeability."
            },
            "structured_decision": {
                "preferred": true,
                "description": "Return a workflow_decision artifact for address_pr_feedback, wait_for_pr_feedback, or mark_ready_to_merge."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "feedback_found_or_no_actionable_feedback_or_ready_to_merge_from_signals",
                "accepted_signals": ["FeedbackFound", "NoFeedbackFound", "PrReadyToMerge", "ChangesRequested", "ChecksFailed"],
                "parent_propagation": "The same activity result is propagated to the parent github_issue_pr workflow."
            },
            "structured_decision": {
                "optional": true,
                "description": "A workflow_decision artifact may update the pr_feedback child workflow, but signals are sufficient."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "on_succeeded": {
                "reducer_next_state": "planning_batch_when_IssueDiscovered_signals_exist_else_idle",
                "accepted_signals": ["IssueDiscovered", "IssueSkipped", "NoOpenIssueFound"],
                "required_summary": "Describe the GitHub issue query, existing workflow checks, and new issue workflow candidates."
            },
            "structured_decision": {
                "optional": true,
                "description": "A workflow_decision artifact may propose a plan_repo_sprint activity command, but IssueDiscovered signals are sufficient."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            }
        }),
        ("repo_backlog", "plan_repo_sprint") => json!({
            "on_succeeded": {
                "reducer_next_state": "dispatching_when_SprintTaskSelected_signals_exist_else_idle",
                "accepted_signals": ["SprintTaskSelected", "IssueSkipped", "NoSprintTaskSelected"],
                "required_summary": "Describe dependency planning, skipped issues, and selected issue workflow dispatch order."
            },
            "structured_decision": {
                "optional": true,
                "description": "A workflow_decision artifact may propose start_child_workflow commands, but SprintTaskSelected signals are sufficient."
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
        (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY) => json!({
            "on_succeeded": {
                "reducer_next_state": "passed",
                "output_signal": QUALITY_PASSED_SIGNAL,
                "required_summary": "Describe validation commands run and passing evidence."
            },
            "on_failed": {
                "reducer_next_state": "failed_or_retry",
                "output_signal": QUALITY_FAILED_SIGNAL,
                "retry_policy": "runtime_retry_policy may retry this activity before failure."
            },
            "on_blocked": {
                "reducer_next_state": "blocked",
                "output_signal": QUALITY_BLOCKED_SIGNAL,
                "required_summary": "Describe the missing dependency, budget, or external input."
            }
        }),
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
        ("github_issue_pr", "sweep_pr_feedback")
        | ("github_issue_pr", PR_FEEDBACK_INSPECT_ACTIVITY)
        | (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => json!({
            "must_include": ["PR comments reviewed", "review states", "check status", "mergeability", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "unverified approval claims"],
            "artifacts": {
                "workflow_decision": {
                    "preferred": true,
                    "allowed_decisions": ["address_pr_feedback", "wait_for_pr_feedback", "mark_ready_to_merge"]
                }
            },
            "signals": {
                "FeedbackFound": "Use when actionable feedback, requested changes, or failed checks require a fix round.",
                "NoFeedbackFound": "Use when no actionable feedback is present yet.",
                "PrReadyToMerge": "Use only when review, checks, and mergeability are all ready."
            }
        }),
        ("repo_backlog", "poll_repo_backlog") => json!({
            "must_include": ["repo and label queried", "open issues inspected", "existing workflow checks", "new issue workflow candidates", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "server-side GitHub polling changes"],
            "signals": {
                "IssueDiscovered": "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available.",
                "IssueSkipped": "Use for open GitHub issues that already have a workflow, are PRs, or should not be started. Include issue_number and reason. When an issue already has a workflow, include workflow_state.",
                "NoOpenIssueFound": "Use when the repo/label query found no candidate issues."
            },
            "artifacts": {
                "workflow_decision": {
                    "optional": true,
                    "allowed_decisions": ["plan_repo_sprint_from_scan", "finish_repo_backlog_scan"]
                }
            }
        }),
        ("repo_backlog", "plan_repo_sprint") => json!({
            "must_include": ["issues considered", "dependency reasoning", "selected tasks", "skipped issues", "next workflow action"],
            "must_not_include": ["repository code changes", "workflow table mutations", "server-side task queue changes"],
            "signals": {
                "SprintTaskSelected": "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers.",
                "IssueSkipped": "Use for discovered issues intentionally skipped by planning. Include issue_number and reason.",
                "NoSprintTaskSelected": "Use when no issue should be dispatched from this sprint plan."
            },
            "artifacts": {
                "sprint_plan": {
                    "optional": true,
                    "fields": ["tasks", "skip"],
                    "task_fields": ["issue", "depends_on"]
                },
                "workflow_decision": {
                    "optional": true,
                    "allowed_decisions": ["start_issue_workflows_from_sprint_plan", "finish_repo_sprint_plan"]
                }
            }
        }),
        (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY) => json!({
            "must_include": ["validation commands", "pass/fail evidence", "remaining blockers"],
            "must_not_include": ["workflow table mutations", "unverified pass claims"],
            "artifacts": {
                "validation_report": {
                    "required_when": "The activity records detailed validation results.",
                    "fields": ["commands", "passed", "failed", "blocked"]
                }
            }
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
        TurnStatus::Completed => match structured_activity_result(items, &activity) {
            StructuredActivityResult::Parsed(result) => result,
            StructuredActivityResult::Missing => ActivityResult::succeeded(activity, summary),
            StructuredActivityResult::Invalid(error) => {
                ActivityResult::failed(activity, "Structured activity result was invalid.", error)
            }
        },
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

enum StructuredActivityResult {
    Missing,
    Parsed(ActivityResult),
    Invalid(String),
}

fn structured_activity_result(items: &[Item], expected_activity: &str) -> StructuredActivityResult {
    let Some(block) = latest_activity_result_block(items) else {
        return StructuredActivityResult::Missing;
    };

    match serde_json::from_str::<ActivityResult>(block) {
        Ok(result) if result.activity == expected_activity => {
            StructuredActivityResult::Parsed(result)
        }
        Ok(result) => StructuredActivityResult::Invalid(format!(
            "activity result block reported activity `{}`, expected `{expected_activity}`",
            result.activity
        )),
        Err(error) => StructuredActivityResult::Invalid(format!(
            "activity result block is invalid JSON: {error}"
        )),
    }
}

fn latest_activity_result_block(items: &[Item]) -> Option<&str> {
    items.iter().rev().find_map(|item| match item {
        Item::AgentReasoning { content } => {
            extract_fenced_block(content, "harness-activity-result")
        }
        _ => None,
    })
}

fn extract_fenced_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let mut result = None;
    let mut offset = 0;
    let mut lines = text.split_inclusive('\n');
    while let Some(line_with_end) = lines.next() {
        let line = line_with_end.trim_end_matches('\n').trim_end_matches('\r');
        if !opening_fence_matches(line, lang) {
            offset += line_with_end.len();
            continue;
        }
        let content_start = offset + line_with_end.len();
        let mut content_end = text.len();
        let mut inner_offset = content_start;
        for inner_line_with_end in lines.by_ref() {
            let inner_line = inner_line_with_end
                .trim_end_matches('\n')
                .trim_end_matches('\r');
            if inner_line.trim().starts_with("```") {
                content_end = inner_offset;
                inner_offset += inner_line_with_end.len();
                break;
            }
            inner_offset += inner_line_with_end.len();
        }
        result = Some(text[content_start..content_end].trim());
        offset = inner_offset;
    }
    result
}

fn opening_fence_matches(line: &str, lang: &str) -> bool {
    let trimmed = line.trim();
    let Some(after_ticks) = trimmed.strip_prefix("```") else {
        return false;
    };
    !after_ticks.starts_with('`') && after_ticks.trim() == lang
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

fn is_builtin_lifecycle_activity(job: &RuntimeJob) -> bool {
    matches!(
        activity_name(job).as_str(),
        "start_child_workflow" | "mark_bound_issue_done" | "recover_issue_workflow"
    )
}

fn required_string<'a>(value: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("start_child_workflow `{field}` is missing"))
}

fn optional_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn string_vec(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn dependency_task_ids_from_command(command: &Value, repo: Option<&str>) -> Vec<TaskId> {
    command
        .get("depends_on")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| dependency_task_id(value, repo))
                .collect()
        })
        .unwrap_or_default()
}

fn dependency_task_id(value: &Value, repo: Option<&str>) -> Option<TaskId> {
    if let Some(issue_number) = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    {
        return Some(TaskId::from_str(&format!(
            "repo-backlog:{}:issue:{issue_number}",
            repo.unwrap_or("<none>")
        )));
    }
    value
        .as_str()
        .filter(|raw| !raw.trim().is_empty())
        .map(TaskId::from_str)
}

fn force_execute_from_project_policy(project_id: &str, labels: &[String]) -> bool {
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(Path::new(project_id))
        .unwrap_or_default();
    labels
        .iter()
        .any(|label| label == &workflow_cfg.issue_workflow.force_execute_label)
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

fn optional_data_string(workflow: &WorkflowInstance, field: &str) -> Option<String> {
    workflow
        .data
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn parse_issue_subject_key(subject_key: &str) -> anyhow::Result<u64> {
    subject_key
        .strip_prefix("issue:")
        .unwrap_or(subject_key)
        .parse::<u64>()
        .with_context(|| format!("start_child_workflow subject_key `{subject_key}` is invalid"))
}

fn parse_pr_subject_key(subject_key: &str) -> Option<u64> {
    subject_key
        .strip_prefix("pr:")
        .unwrap_or(subject_key)
        .parse::<u64>()
        .ok()
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

struct PrFeedbackChildData<'a> {
    project_id: &'a str,
    repo: Option<&'a str>,
    issue_number: Option<u64>,
    pr_number: u64,
    pr_url: Option<&'a str>,
    parent_workflow_id: &'a str,
    runtime_job_id: &'a str,
    command_id: &'a str,
}

fn merge_pr_feedback_child_data(mut data: Value, input: PrFeedbackChildData<'_>) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    if let Some(object) = data.as_object_mut() {
        object.insert("project_id".to_string(), json!(input.project_id));
        object.insert("repo".to_string(), json!(input.repo));
        object.insert("issue_number".to_string(), json!(input.issue_number));
        object.insert("pr_number".to_string(), json!(input.pr_number));
        object.insert("pr_url".to_string(), json!(input.pr_url));
        object.insert(
            "parent_workflow_id".to_string(),
            json!(input.parent_workflow_id),
        );
        object.insert(
            "started_by_runtime_job_id".to_string(),
            json!(input.runtime_job_id),
        );
        object.insert("started_by_command_id".to_string(), json!(input.command_id));
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(Path::new(input.project_id), data)
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

    #[test]
    fn dependency_task_ids_from_command_maps_issue_numbers_to_repo_handles() {
        let command = json!({
            "depends_on": [42, "43", "explicit-task-id"]
        });

        let dependencies = dependency_task_ids_from_command(&command, Some("owner/repo"));

        assert_eq!(
            dependencies
                .iter()
                .map(|task_id| task_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "repo-backlog:owner/repo:issue:42",
                "repo-backlog:owner/repo:issue:43",
                "explicit-task-id"
            ]
        );
    }

    #[test]
    fn activity_result_schema_describes_quality_gate_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": QUALITY_GATE_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            QUALITY_GATE_DEFINITION_ID,
            1,
            "checking",
            WorkflowSubject::new("quality_gate", "issue:123"),
        )
        .with_id("quality-gate-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], QUALITY_GATE_DEFINITION_ID);
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
            "passed"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["output_signal"],
            QUALITY_PASSED_SIGNAL
        );
        assert_eq!(
            schema["agent_summary_contract"]["must_include"][0],
            "validation commands"
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "passed"));
    }

    #[test]
    fn activity_result_schema_describes_repo_backlog_poll_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "poll_repo_backlog"
            }),
        );
        let workflow = WorkflowInstance::new(
            "repo_backlog",
            1,
            "scanning",
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id("repo-backlog-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], "repo_backlog");
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "IssueDiscovered"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["IssueDiscovered"],
            "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "planning_batch"));
    }

    #[test]
    fn activity_result_schema_describes_repo_sprint_plan_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": harness_workflow::runtime::REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            "repo_backlog",
            1,
            "planning_batch",
            WorkflowSubject::new("repo", "owner/repo"),
        )
        .with_id("repo-backlog-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], "repo_backlog");
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "SprintTaskSelected"
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["SprintTaskSelected"],
            "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "dispatching"));
    }

    #[test]
    fn activity_result_schema_describes_pr_feedback_child_contract() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": PR_FEEDBACK_INSPECT_ACTIVITY
            }),
        );
        let workflow = WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "inspecting",
            WorkflowSubject::new("pr", "pr:77"),
        )
        .with_id("pr-feedback-1");

        let schema = activity_result_schema(&job, Some(&workflow));

        assert_eq!(schema["workflow_definition"], PR_FEEDBACK_DEFINITION_ID);
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
            "FeedbackFound"
        );
        assert_eq!(
            schema["transition_contract"]["on_succeeded"]["parent_propagation"],
            "The same activity result is propagated to the parent github_issue_pr workflow."
        );
        assert_eq!(
            schema["agent_summary_contract"]["signals"]["PrReadyToMerge"],
            "Use only when review, checks, and mergeability are all ready."
        );
        assert!(schema["workflow_decision_contract"]["allowed_transitions"]
            .as_array()
            .expect("allowed transitions should be an array")
            .iter()
            .any(|transition| transition["next_state"] == "feedback_found"));
    }

    #[test]
    fn activity_result_from_turn_parses_structured_activity_result_block() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Succeeded
        );
        assert_eq!(result.summary, "Implementation completed.");
        let pr_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "pull_request")
            .expect("structured pull request artifact should be preserved");
        assert_eq!(pr_artifact.artifact["pr_number"], 77);
        assert_eq!(
            pr_artifact.artifact["pr_url"],
            "https://github.com/owner/repo/pull/77"
        );
        let prompt_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "runtime_prompt_packet")
            .expect("runtime prompt artifact should be appended");
        assert_eq!(prompt_artifact.artifact["digest"], "digest-1");
        let turn_artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "runtime_turn")
            .expect("runtime turn artifact should be appended");
        assert_eq!(turn_artifact.artifact["thread_id"], "thread-1");
        assert_eq!(turn_artifact.artifact["turn_id"], "turn-1");
        assert!(result
            .signals
            .iter()
            .any(|signal| signal.signal_type == "RuntimeTurnCompleted"));
    }

    #[test]
    fn activity_result_from_turn_fails_mismatched_structured_activity() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let content = r#"Wrong activity result.

```harness-activity-result
{"activity":"replan_issue","status":"succeeded","summary":"Wrong activity.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#;
        let items = vec![Item::AgentReasoning {
            content: content.to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(result.summary, "Structured activity result was invalid.");
        assert_eq!(
            result.error.as_deref(),
            Some("activity result block reported activity `replan_issue`, expected `implement_issue`")
        );
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_turn"));
    }

    #[test]
    fn activity_result_from_turn_fails_latest_malformed_structured_activity() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue"
            }),
        );
        let items = vec![Item::AgentReasoning {
            content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":
```"#
                .to_string(),
        }];

        let result = activity_result_from_turn(
            &job,
            &TurnStatus::Completed,
            &items,
            &ThreadId::from_str("thread-1"),
            &TurnId::from_str("turn-1"),
            "codex",
            Path::new("/project"),
            "digest-1",
        );

        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(result.summary, "Structured activity result was invalid.");
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("activity result block is invalid JSON:")));
        assert!(!result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "runtime_turn"));
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
    fn runtime_issue_completion_task_preserves_intake_identity() {
        let instance = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "cancelled",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_data(json!({
            "task_id": "runtime-task-42",
            "project_id": "/tmp/project",
            "repo": "owner/repo",
            "issue_number": 42,
            "source": "github",
            "external_id": "issue:42",
        }));
        let result = ActivityResult::cancelled("implement_issue", "Runtime job was cancelled.");

        let task = runtime_issue_completion_task(&instance, Some(&result))
            .expect("terminal runtime issue should map to an intake task");

        assert_eq!(task.id.as_str(), "runtime-task-42");
        assert_eq!(task.task_kind, TaskKind::Issue);
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.source.as_deref(), Some("github"));
        assert_eq!(task.external_id.as_deref(), Some("issue:42"));
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
        assert_eq!(task.issue, Some(42));
    }

    #[test]
    fn runtime_issue_completion_task_marks_retryable_failures_as_transient() {
        let instance = WorkflowInstance::new(
            "github_issue_pr",
            1,
            "failed",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_data(json!({
            "task_id": "runtime-task-42",
            "project_id": "/tmp/project",
            "repo": "owner/repo",
            "issue_number": 42,
            "source": "github",
            "external_id": "issue:42",
        }));
        let result = ActivityResult::failed(
            "implement_issue",
            "Runtime dependency failed.",
            "provider temporarily unavailable",
        )
        .with_error_kind(ActivityErrorKind::ExternalDependency);

        let task = runtime_issue_completion_task(&instance, Some(&result))
            .expect("terminal runtime issue should map to an intake task");

        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.failure_kind, Some(TaskFailureKind::WorkspaceLifecycle));
        assert_eq!(
            task.error.as_deref(),
            Some("provider temporarily unavailable")
        );
    }

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
