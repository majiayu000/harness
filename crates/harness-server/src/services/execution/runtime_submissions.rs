//! Workflow-runtime persistence for accepted submissions.

use super::*;

impl DefaultExecutionService {
    pub(super) async fn submit_issue_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimeIssueSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for GitHub issue submissions".to_string(),
            ));
        };
        let Some(issue_number) = prepared.req.issue else {
            return Err(EnqueueTaskError::BadRequest(
                "issue submission requires an issue number".to_string(),
            ));
        };
        let dependencies_blocked = self.dependencies_blocked(&prepared.req).await?
            || self
                .serialization_dependencies_blocked(&prepared.req)
                .await?;

        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_issue_submission(
            store,
            crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                project_root,
                repo: prepared.req.repo.as_deref(),
                issue_number,
                task_id: &task_id,
                labels: &prepared.req.labels,
                force_execute: prepared.req.force_execute,
                additional_prompt: prepared.req.prompt.as_deref(),
                depends_on: &prepared.req.depends_on,
                dependencies_blocked,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
                remote_fact_hash: None,
                author_trust_class: None,
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected issue submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() && !dependencies_blocked {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted issue submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        tracing::info!(
            workflow_id = %record.workflow_id,
            task_id = %task_id.0,
            command_count = record.command_ids.len(),
            "execution service: accepted issue submission into workflow runtime"
        );
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    pub(super) async fn submit_prompt_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimePromptSubmission,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for prompt submissions".to_string(),
            ));
        };
        let Some(prompt) = prepared.req.prompt.as_deref() else {
            return Err(EnqueueTaskError::BadRequest(
                "prompt submission requires a prompt".to_string(),
            ));
        };
        let dependencies_blocked = self.dependencies_blocked(&prepared.req).await?;

        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let execution_policy =
            crate::workflow_runtime_submission::runtime_models::PromptExecutionPolicy {
                task_kind: prepared.req.task_kind(),
                agent: prepared.req.agent.clone(),
                turn_timeout_secs: Some(prepared.req.turn_timeout_secs),
                queue_domain,
                priority: prepared.req.priority,
            };
        let record = crate::workflow_runtime_submission::record_prompt_submission_with_policy(
            store,
            crate::workflow_runtime_submission::PromptSubmissionRuntimeContext {
                project_root,
                task_id: &task_id,
                prompt,
                depends_on: &prepared.req.depends_on,
                serialization_depends_on: &prepared.req.serialization_depends_on,
                dependencies_blocked,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
                continuation: prepared.req.continuation.as_ref(),
            },
            execution_policy,
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected prompt submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() && !dependencies_blocked {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted prompt submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        tracing::info!(
            workflow_id = %record.workflow_id,
            task_id = %task_id.0,
            command_count = record.command_ids.len(),
            "execution service: accepted prompt submission into workflow runtime"
        );
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    pub(super) async fn submit_declarative_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimeDeclarativeSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for declarative workflow submissions"
                    .to_string(),
            ));
        };
        let Some(definition_id) = prepared.req.definition_id.as_deref() else {
            return Err(EnqueueTaskError::BadRequest(
                "declarative workflow submission requires definition_id".to_string(),
            ));
        };
        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_declarative_submission(
            store,
            crate::workflow_runtime_submission::DeclarativeSubmissionRuntimeContext {
                project_root,
                definition_id,
                task_id: &task_id,
                prompt: prepared.req.prompt.as_deref().ok_or_else(|| {
                    EnqueueTaskError::BadRequest(
                        "declarative workflow submissions require a non-empty prompt".to_string(),
                    )
                })?,
                depends_on: &prepared.req.depends_on,
                serialization_depends_on: &prepared.req.serialization_depends_on,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
                subject_key: None,
                repo: prepared.req.repo.as_deref(),
                author_trust_class: None,
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;
        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected declarative submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted declarative submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    pub(super) async fn submit_pr_feedback_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimePrFeedbackSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for PR feedback submissions".to_string(),
            ));
        };
        let Some(pr_number) = prepared.req.pr else {
            return Err(EnqueueTaskError::BadRequest(
                "PR feedback submission requires a PR number".to_string(),
            ));
        };

        let maybe_instance = self
            .find_runtime_issue_workflow_by_pr(
                store,
                &prepared.project_id,
                prepared.req.repo.as_deref(),
                pr_number,
            )
            .await?;

        let outcome = if let Some(instance) = maybe_instance {
            if instance.state == "pr_open" {
                crate::workflow_runtime_pr_feedback::request_local_review(store, &instance.id).await
            } else {
                crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep(store, &instance.id)
                    .await
            }
        } else {
            let task_id = crate::workflow_runtime_pr_feedback::synthesized_pr_feedback_task_id(
                &prepared.project_id,
                prepared.req.repo.as_deref(),
                pr_number,
            );
            crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep_for_pr(
                store,
                crate::workflow_runtime_pr_feedback::PrFeedbackSweepRuntimeContext {
                    project_root: std::path::Path::new(&prepared.project_id),
                    repo: prepared.req.repo.as_deref(),
                    task_id: &task_id,
                    pr_number,
                    pr_url: None,
                },
            )
            .await
        }
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        match outcome {
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested {
                workflow_id,
                task_id,
                ..
            }
            | crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                workflow_id,
                task_id,
                ..
            } => {
                runtime_submission_response_handle(store, &workflow_id, &TaskId::from_str(&task_id))
                    .await
            }
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::NotCandidate {
                workflow_id,
                state,
            } => Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected PR feedback submission for workflow {workflow_id}: state {state} is not eligible for feedback sweep"
            ))),
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id,
                reason,
            } => Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected PR feedback submission for workflow {workflow_id}: {reason}"
            ))),
        }
    }

    pub(super) async fn find_runtime_issue_workflow_by_pr(
        &self,
        store: &harness_workflow::runtime::WorkflowRuntimeStore,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> Result<Option<harness_workflow::runtime::WorkflowInstance>, EnqueueTaskError> {
        store
            .get_instance_by_pr(GITHUB_ISSUE_PR_DEFINITION_ID, project_id, repo, pr_number)
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))
    }
}
