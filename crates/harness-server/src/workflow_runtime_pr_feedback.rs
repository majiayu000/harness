use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_sweep_decision,
    DecisionValidator, PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackOutcome,
    PrFeedbackSweepDecisionInput, ValidationContext, WorkflowDecision, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowEvidence, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::json;
use std::path::Path;

pub(crate) struct PrDetectedRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: &'a str,
}

pub(crate) struct PrFeedbackRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub outcome: PrFeedbackOutcome,
    pub summary: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrFeedbackSweepRequestOutcome {
    Requested { workflow_id: String },
    NotCandidate { workflow_id: String, state: String },
    ActiveCommandExists { workflow_id: String },
    Rejected { workflow_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeMergeApprovalOutcome {
    Approved {
        workflow_id: String,
    },
    NotFound,
    NotCandidate {
        workflow_id: String,
        definition_id: String,
    },
    NotReady {
        workflow_id: String,
        state: String,
    },
    Rejected {
        workflow_id: String,
        reason: String,
    },
}

pub(crate) async fn record_pr_detected(
    store: Option<&WorkflowRuntimeStore>,
    ctx: PrDetectedRuntimeContext<'_>,
) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) = persist_pr_detected(store, &ctx).await {
        tracing::warn!(
            issue = ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR detection write failed: {error}"
        );
    }
}

pub(crate) async fn record_pr_feedback(
    store: Option<&WorkflowRuntimeStore>,
    ctx: PrFeedbackRuntimeContext<'_>,
) {
    let Some(store) = store else {
        return;
    };
    let Some(issue_number) = ctx.issue_number else {
        return;
    };
    if let Err(error) = persist_pr_feedback(store, &ctx, issue_number).await {
        tracing::warn!(
            issue = issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR feedback write failed: {error}"
        );
    }
}

pub(crate) async fn request_pr_feedback_sweep(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        anyhow::bail!("workflow runtime instance `{workflow_id}` was not found");
    };
    if instance.definition_id != "github_issue_pr"
        || !matches!(instance.state.as_str(), "pr_open" | "awaiting_feedback")
    {
        return Ok(PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id: instance.id,
            state: instance.state,
        });
    }
    if has_active_pr_feedback_command(store, &instance.id).await? {
        return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: instance.id,
        });
    }
    persist_pr_feedback_sweep_request(store, instance).await
}

pub(crate) async fn approve_runtime_merge_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &str,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    let Some(instance) = store.get_instance_by_task_id(task_id).await? else {
        return Ok(RuntimeMergeApprovalOutcome::NotFound);
    };
    approve_runtime_merge(store, instance, Some(task_id)).await
}

pub(crate) async fn approve_runtime_merge_by_workflow_id(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(RuntimeMergeApprovalOutcome::NotFound);
    };
    approve_runtime_merge(store, instance, None).await
}

async fn persist_pr_detected(
    store: &WorkflowRuntimeStore,
    ctx: &PrDetectedRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, ctx.issue_number);
    upsert_github_issue_pr_definition(store).await?;
    let mut instance = load_or_issue_instance(
        store,
        workflow_id,
        project_id.clone(),
        ctx.repo.map(ToOwned::to_owned),
        ctx.issue_number,
        "implementing",
    )
    .await?;
    instance.data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
        "project_id": project_id,
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            "PrDetected",
            "workflow_runtime_pr_feedback",
            json!({
                "task_id": ctx.task_id.as_str(),
                "issue_number": ctx.issue_number,
                "repo": ctx.repo,
                "pr_number": ctx.pr_number,
                "pr_url": ctx.pr_url,
            }),
        )
        .await?;
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: ctx.task_id.as_str(),
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
        },
    );
    apply_decision(store, instance, output.decision, Some(event.id)).await
}

async fn persist_pr_feedback_sweep_request(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let pr_number = required_u64_field(&instance.data, "pr_number")?;
    let pr_url = optional_string_field(&instance.data, "pr_url");
    let issue_number = instance
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let repo = optional_string_field(&instance.data, "repo");
    let event = store
        .append_event(
            &instance.id,
            "PrFeedbackSweepRequested",
            "workflow_runtime_pr_feedback",
            json!({
                "issue_number": issue_number,
                "repo": repo.as_deref(),
                "pr_number": pr_number,
                "pr_url": pr_url.as_deref(),
            }),
        )
        .await?;
    let output = build_pr_feedback_sweep_decision(
        &instance,
        PrFeedbackSweepDecisionInput {
            dedupe_key: &format!("pr-feedback-sweep:{}:{}", instance.id, event.id),
            pr_number,
            pr_url: pr_url.as_deref(),
            issue_number,
            repo: repo.as_deref(),
            summary: "Runtime workflow requested a PR feedback sweep.",
        },
    );
    let validator = DecisionValidator::github_issue_pr();
    let validation = validator.validate(
        &instance,
        &output.decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event.id)),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(output.decision, Some(event.id), &reason);
            store.record_decision(&record).await?;
            return Ok(PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id: instance.id,
                reason,
            });
        }
    };
    store.record_decision(&record).await?;
    for command in &output.decision.commands {
        store
            .enqueue_command(&instance.id, Some(&record.id), command)
            .await?;
    }
    instance.state = output.decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(instance.data, &output.decision.decision);
    store.upsert_instance(&instance).await?;
    Ok(PrFeedbackSweepRequestOutcome::Requested {
        workflow_id: instance.id,
    })
}

async fn persist_pr_feedback(
    store: &WorkflowRuntimeStore,
    ctx: &PrFeedbackRuntimeContext<'_>,
    issue_number: u64,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, issue_number);
    upsert_github_issue_pr_definition(store).await?;
    let mut instance = load_or_issue_instance(
        store,
        workflow_id,
        project_id.clone(),
        ctx.repo.map(ToOwned::to_owned),
        issue_number,
        "pr_open",
    )
    .await?;
    instance.data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
        "project_id": project_id,
        "repo": ctx.repo,
        "issue_number": issue_number,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
        "feedback_summary": ctx.summary,
        }),
    );
    store.upsert_instance(&instance).await?;
    let event = store
        .append_event(
            &instance.id,
            event_type(ctx.outcome),
            "workflow_runtime_pr_feedback",
            json!({
                "task_id": ctx.task_id.as_str(),
                "issue_number": issue_number,
                "repo": ctx.repo,
                "pr_number": ctx.pr_number,
                "pr_url": ctx.pr_url,
                "outcome": outcome_label(ctx.outcome),
                "summary": ctx.summary,
            }),
        )
        .await?;
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: ctx.task_id.as_str(),
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
            outcome: ctx.outcome,
            summary: ctx.summary,
        },
    );
    apply_decision(store, instance, output.decision, Some(event.id)).await
}

async fn approve_runtime_merge(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    task_id: Option<&str>,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    if instance.definition_id != harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID {
        return Ok(RuntimeMergeApprovalOutcome::NotCandidate {
            workflow_id: instance.id,
            definition_id: instance.definition_id,
        });
    }
    if instance.state != "ready_to_merge" {
        return Ok(RuntimeMergeApprovalOutcome::NotReady {
            workflow_id: instance.id,
            state: instance.state,
        });
    }

    let event = store
        .append_event(
            &instance.id,
            "MergeApproved",
            "workflow_runtime_dashboard",
            json!({
                "task_id": task_id,
                "issue_number": instance.data.get("issue_number").and_then(|value| value.as_u64()),
                "repo": optional_string_field(&instance.data, "repo"),
                "pr_number": instance.data.get("pr_number").and_then(|value| value.as_u64()),
                "pr_url": optional_string_field(&instance.data, "pr_url"),
            }),
        )
        .await?;
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "approve_merge",
        "done",
        "operator approved the ready-to-merge workflow",
    )
    .with_command(harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::MarkDone,
        format!("merge-approved:{}:{}", instance.id, event.id),
        json!({
            "workflow_id": instance.id,
            "task_id": task_id,
            "approved_by": "dashboard",
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "operator_approval",
        "dashboard merge approval for ready-to-merge workflow",
    ))
    .high_confidence();
    let validator = DecisionValidator::github_issue_pr();
    let validation = validator.validate(
        &instance,
        &decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id)),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(decision, Some(event.id), &reason);
            store.record_decision(&record).await?;
            return Ok(RuntimeMergeApprovalOutcome::Rejected {
                workflow_id: instance.id,
                reason,
            });
        }
    };
    store.record_decision(&record).await?;
    for command in &decision.commands {
        store
            .enqueue_command(&instance.id, Some(&record.id), command)
            .await?;
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_runtime_merge_data(instance.data, &decision.decision, task_id);
    let workflow_id = instance.id.clone();
    store.upsert_instance(&instance).await?;
    Ok(RuntimeMergeApprovalOutcome::Approved { workflow_id })
}

async fn apply_decision(
    store: &WorkflowRuntimeStore,
    mut instance: WorkflowInstance,
    decision: WorkflowDecision,
    event_id: Option<String>,
) -> anyhow::Result<()> {
    let validator = DecisionValidator::github_issue_pr();
    let validation = validator.validate(
        &instance,
        &decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    let record = match validation {
        Ok(()) => WorkflowDecisionRecord::accepted(decision.clone(), event_id),
        Err(error) => {
            let reason = error.to_string();
            let record = WorkflowDecisionRecord::rejected(decision, event_id, &reason);
            store.record_decision(&record).await?;
            return Ok(());
        }
    };
    store.record_decision(&record).await?;
    for command in &decision.commands {
        store
            .enqueue_command(&instance.id, Some(&record.id), command)
            .await?;
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(instance.data, &decision.decision);
    store.upsert_instance(&instance).await
}

async fn has_active_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let parent_has_active_command =
        store
            .commands_for(workflow_id)
            .await?
            .into_iter()
            .any(|record| {
                matches!(record.status.as_str(), "pending" | "dispatched")
                    && matches!(
                        record.command.activity_name(),
                        Some("sweep_pr_feedback" | "address_pr_feedback")
                    )
                    || matches!(record.status.as_str(), "pending" | "dispatched")
                        && record.command.command_type
                            == harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
                        && record
                            .command
                            .command
                            .get("definition_id")
                            .and_then(|value| value.as_str())
                            == Some(PR_FEEDBACK_DEFINITION_ID)
            });
    if parent_has_active_command {
        return Ok(true);
    }

    for instance in store
        .list_instances_by_definition(PR_FEEDBACK_DEFINITION_ID, None, None)
        .await?
        .into_iter()
        .filter(|instance| instance.parent_workflow_id.as_deref() == Some(workflow_id))
    {
        if matches!(instance.state.as_str(), "pending" | "inspecting")
            && has_active_child_pr_feedback_command(store, &instance.id).await?
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn has_active_child_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            matches!(record.status.as_str(), "pending" | "dispatched")
                && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
        }))
}

async fn upsert_github_issue_pr_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            "github_issue_pr",
            1,
            "GitHub issue PR workflow",
        ))
        .await
}

async fn load_or_issue_instance(
    store: &WorkflowRuntimeStore,
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> anyhow::Result<WorkflowInstance> {
    Ok(match store.get_instance(&workflow_id).await? {
        Some(instance) => instance,
        None => issue_instance(workflow_id, project_id, repo, issue_number, state),
    })
}

fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(crate::workflow_runtime_policy::merge_runtime_retry_policy(
        Path::new(&project_id),
        json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
        }),
    ))
}

fn merge_last_decision(mut data: serde_json::Value, decision: &str) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
    }
    data
}

fn merge_runtime_merge_data(
    mut data: serde_json::Value,
    decision: &str,
    task_id: Option<&str>,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert("merge_approved_at".to_string(), json!(chrono::Utc::now()));
        if let Some(task_id) = task_id {
            object.insert("merge_approved_task_id".to_string(), json!(task_id));
        }
    }
    data
}

fn optional_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn required_u64_field(data: &serde_json::Value, field: &str) -> anyhow::Result<u64> {
    data.get(field)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing {field}"))
}

fn event_type(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "FeedbackFound",
        PrFeedbackOutcome::NoActionableFeedback => "NoFeedbackFound",
        PrFeedbackOutcome::ReadyToMerge => "PrReadyToMerge",
    }
}

fn outcome_label(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "blocking_feedback",
        PrFeedbackOutcome::NoActionableFeedback => "no_actionable_feedback",
        PrFeedbackOutcome::ReadyToMerge => "ready_to_merge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;

    #[tokio::test]
    async fn pr_detected_persists_pr_open_state() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => store,
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("task-1");

        record_pr_detected(
            Some(&store),
            PrDetectedRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 123,
                task_id: &task_id,
                pr_number: 77,
                pr_url: "https://github.com/owner/repo/pull/77",
            },
        )
        .await;

        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should be persisted");
        assert_eq!(instance.state, "pr_open");
        assert_eq!(
            store.events_for(&workflow_id).await?[0].event_type,
            "PrDetected"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pr_feedback_ready_to_merge_updates_parent_workflow() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => store,
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("task-1");
        record_pr_detected(
            Some(&store),
            PrDetectedRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: 123,
                task_id: &task_id,
                pr_number: 77,
                pr_url: "https://github.com/owner/repo/pull/77",
            },
        )
        .await;

        record_pr_feedback(
            Some(&store),
            PrFeedbackRuntimeContext {
                project_root: &project_root,
                repo: Some("owner/repo"),
                issue_number: Some(123),
                task_id: &task_id,
                pr_number: 77,
                pr_url: Some("https://github.com/owner/repo/pull/77"),
                outcome: PrFeedbackOutcome::ReadyToMerge,
                summary: "Reviewer approved and validation passed.",
            },
        )
        .await;

        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        let instance = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should be persisted");
        assert_eq!(instance.state, "ready_to_merge");
        let events = store.events_for(&workflow_id).await?;
        assert!(events
            .iter()
            .any(|event| event.event_type == "PrReadyToMerge"));
        Ok(())
    }

    #[tokio::test]
    async fn request_pr_feedback_sweep_records_runtime_command() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => store,
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        upsert_github_issue_pr_definition(&store).await?;
        let instance = issue_instance(
            workflow_id.clone(),
            project_root.to_string_lossy().into_owned(),
            Some("owner/repo".to_string()),
            123,
            "pr_open",
        )
        .with_data(json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 123,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "task-1",
        }));
        store.upsert_instance(&instance).await?;

        let outcome = request_pr_feedback_sweep(&store, &workflow_id).await?;
        assert_eq!(
            outcome,
            PrFeedbackSweepRequestOutcome::Requested {
                workflow_id: workflow_id.clone()
            }
        );
        let updated = store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow should still exist");
        assert_eq!(updated.state, "awaiting_feedback");
        assert_eq!(updated.data["last_decision"], "sweep_pr_feedback");
        let commands = store.commands_for(&workflow_id).await?;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].status, "pending");
        assert_eq!(
            commands[0].command.command_type,
            harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
        );
        assert_eq!(
            commands[0].command.command["definition_id"],
            PR_FEEDBACK_DEFINITION_ID
        );
        assert_eq!(
            commands[0].command.command["child_activity"],
            harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY
        );
        assert_eq!(commands[0].command.command["pr_number"], 77);

        let second = request_pr_feedback_sweep(&store, &workflow_id).await?;
        assert_eq!(
            second,
            PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                workflow_id: workflow_id.clone()
            }
        );
        assert_eq!(store.commands_for(&workflow_id).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn completed_inspecting_child_does_not_block_next_feedback_sweep() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => store,
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        upsert_github_issue_pr_definition(&store).await?;
        let parent = issue_instance(
            workflow_id.clone(),
            project_root.to_string_lossy().into_owned(),
            Some("owner/repo".to_string()),
            123,
            "awaiting_feedback",
        );
        store.upsert_instance(&parent).await?;
        let child = WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "inspecting",
            WorkflowSubject::new("pr", "pr:77"),
        )
        .with_id("pr-feedback-child-completed")
        .with_parent(workflow_id.clone());
        store.upsert_instance(&child).await?;

        assert!(
            !has_active_pr_feedback_command(&store, &workflow_id).await?,
            "an inspecting child with no active command should not suppress future sweeps"
        );

        let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "inspect-pr-feedback-77",
        );
        store.enqueue_command(&child.id, None, &command).await?;

        assert!(
            has_active_pr_feedback_command(&store, &workflow_id).await?,
            "an inspecting child with a pending command should still suppress duplicate sweeps"
        );
        Ok(())
    }

    #[tokio::test]
    async fn orphan_pending_child_does_not_block_next_feedback_sweep() -> anyhow::Result<()> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let store =
            match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url))
                .await
            {
                Ok(store) => store,
                Err(_) => return Ok(()),
            };
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        upsert_github_issue_pr_definition(&store).await?;
        let parent = issue_instance(
            workflow_id.clone(),
            project_root.to_string_lossy().into_owned(),
            Some("owner/repo".to_string()),
            123,
            "awaiting_feedback",
        )
        .with_data(json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 123,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "task_id": "task-1",
        }));
        store.upsert_instance(&parent).await?;
        let child = WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "pending",
            WorkflowSubject::new("pr", "pr:77"),
        )
        .with_id("pr-feedback-child-orphan")
        .with_parent(workflow_id.clone());
        store.upsert_instance(&child).await?;

        assert!(
            !has_active_pr_feedback_command(&store, &workflow_id).await?,
            "a pending child without an active inspection command should not suppress future sweeps"
        );

        let child_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
            PR_FEEDBACK_INSPECT_ACTIVITY,
            "inspect-pr-feedback-77",
        );
        let child_command_id = store
            .enqueue_command(&child.id, None, &child_command)
            .await?;

        assert!(
            has_active_pr_feedback_command(&store, &workflow_id).await?,
            "a pending child with an active inspection command should still suppress duplicate sweeps"
        );

        store
            .mark_command_status(&child_command_id, "completed")
            .await?;
        assert!(
            !has_active_pr_feedback_command(&store, &workflow_id).await?,
            "a pending child with no active inspection command should stop suppressing sweeps"
        );

        let outcome = request_pr_feedback_sweep(&store, &workflow_id).await?;
        assert_eq!(
            outcome,
            PrFeedbackSweepRequestOutcome::Requested {
                workflow_id: workflow_id.clone()
            }
        );
        Ok(())
    }
}
