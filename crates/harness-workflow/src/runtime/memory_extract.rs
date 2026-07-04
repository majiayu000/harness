use super::model::{
    ActivityErrorKind, ActivityResult, ActivitySignal, ValidationRecord, WorkflowDecisionRecord,
    WorkflowEvent, WorkflowInstance,
};
use super::repo_memory::{RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord};
use super::state_registry::WorkflowTerminalState;
use super::store::WorkflowRuntimeStore;
use serde_json::{json, Value};

impl WorkflowRuntimeStore {
    pub(crate) async fn record_terminal_repo_memory_for_completion(
        &self,
        event: &WorkflowEvent,
        decision: &WorkflowDecisionRecord,
    ) {
        if let Err(error) = self
            .try_record_terminal_repo_memory_for_completion(event, decision)
            .await
        {
            tracing::error!(
                workflow_id = %decision.workflow_id,
                event_id = %event.id,
                error = %error,
                "workflow repo memory extraction failed"
            );
        }
    }

    async fn try_record_terminal_repo_memory_for_completion(
        &self,
        event: &WorkflowEvent,
        decision: &WorkflowDecisionRecord,
    ) -> anyhow::Result<()> {
        if !decision.accepted {
            return Ok(());
        }
        let Some(instance) = self.get_instance(&decision.workflow_id).await? else {
            return Ok(());
        };
        let Some(record) = extract_terminal_repo_memory_record(&instance, event, decision)? else {
            return Ok(());
        };
        self.insert_repo_memory_record(&record).await?;
        Ok(())
    }
}

fn extract_terminal_repo_memory_record(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    decision: &WorkflowDecisionRecord,
) -> anyhow::Result<Option<RepoMemoryRecord>> {
    if !decision.accepted {
        return Ok(None);
    }
    let Some(outcome) = repo_memory_outcome(instance) else {
        return Ok(None);
    };
    let result: ActivityResult =
        serde_json::from_value(event.event.get("activity_result").cloned().ok_or_else(|| {
            anyhow::anyhow!("RuntimeJobCompleted event missing activity_result")
        })?)?;
    let Some(repo) = repo_from_completion(instance, event, &result) else {
        return Ok(None);
    };
    let Some(activity_class) =
        clean_string(&result.activity).or_else(|| clean_string(&decision.decision.decision))
    else {
        return Ok(None);
    };

    let extracted = if outcome == RepoMemoryOutcome::Failed {
        Some((
            RepoMemoryKind::FailureLesson,
            failure_lesson_payload(instance, &result, decision)?,
        ))
    } else if let Some(payload) = validation_command_payload(instance, &result, decision) {
        Some((RepoMemoryKind::ValidationCommand, payload))
    } else {
        feedback_pattern_payload(instance, &result, decision)
            .map(|payload| (RepoMemoryKind::ReviewerPattern, payload))
    };
    let Some((kind, payload_json)) = extracted else {
        return Ok(None);
    };

    Ok(Some(
        RepoMemoryRecord::new(repo, activity_class, outcome, kind, payload_json)
            .with_evidence_ref(format!("workflow:{}:event:{}", instance.id, event.id)),
    ))
}

fn repo_memory_outcome(instance: &WorkflowInstance) -> Option<RepoMemoryOutcome> {
    match (instance.state.as_str(), instance.terminal_state()?) {
        ("done", WorkflowTerminalState::Succeeded) => Some(RepoMemoryOutcome::Done),
        ("failed", WorkflowTerminalState::Failed) => Some(RepoMemoryOutcome::Failed),
        _ => None,
    }
}

fn repo_from_completion(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<String> {
    clean_json_string(instance.data.get("repo"))
        .or_else(|| clean_json_string(event.event.get("repo")))
        .or_else(|| clean_json_string(event.event.pointer("/command/command/repo")))
        .or_else(|| {
            result
                .signals
                .iter()
                .find_map(|signal| clean_json_string(signal.signal.get("repo")))
        })
}

fn validation_command_payload(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    decision: &WorkflowDecisionRecord,
) -> Option<Value> {
    let validation: Vec<Value> = result
        .validation
        .iter()
        .filter_map(validation_record_payload)
        .collect();
    if validation.is_empty() {
        return None;
    }
    Some(common_payload(
        instance,
        result,
        decision,
        json!({ "validation": validation }),
    ))
}

fn validation_record_payload(record: &ValidationRecord) -> Option<Value> {
    let command = clean_string(&record.command)?;
    let status = clean_string(&record.status)?;
    if !validation_status_was_run(&status) {
        return None;
    }
    let mut payload = json!({
        "command": command,
        "status": status,
    });
    if let Some(reason) = record.reason.as_deref().and_then(clean_string) {
        if let Some(object) = payload.as_object_mut() {
            object.insert("reason".to_string(), json!(reason));
        }
    }
    Some(payload)
}

fn validation_status_was_run(status: &str) -> bool {
    !matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "" | "skip" | "skipped" | "not_run" | "not-run" | "not_applicable" | "not-applicable"
    )
}

fn failure_lesson_payload(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    decision: &WorkflowDecisionRecord,
) -> anyhow::Result<Value> {
    let failure_class =
        serde_json::to_value(result.error_kind.unwrap_or(ActivityErrorKind::Unknown))?;
    Ok(common_payload(
        instance,
        result,
        decision,
        json!({
            "failure_class": failure_class,
            "activity_status": result.status,
            "has_error_message": result.error.as_deref().is_some_and(|error| !error.trim().is_empty()),
        }),
    ))
}

fn feedback_pattern_payload(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    decision: &WorkflowDecisionRecord,
) -> Option<Value> {
    let mut signals = Vec::new();
    let mut pr_numbers = Vec::new();
    let mut saw_blocking = false;
    let mut saw_ready = false;
    let mut saw_no_actionable = false;

    for signal in &result.signals {
        let Some(category) = feedback_category_for_signal(signal) else {
            continue;
        };
        signals.push(signal.signal_type.clone());
        if let Some(pr_number) = signal.signal.get("pr_number").and_then(Value::as_u64) {
            pr_numbers.push(pr_number);
        }
        match category {
            "blocking_feedback" => saw_blocking = true,
            "ready_to_merge" => saw_ready = true,
            "no_actionable_feedback" => saw_no_actionable = true,
            _ => {}
        }
    }
    if signals.is_empty() {
        return None;
    }
    signals.sort();
    signals.dedup();
    pr_numbers.sort_unstable();
    pr_numbers.dedup();
    let feedback_category = if saw_blocking {
        "blocking_feedback"
    } else if saw_ready {
        "ready_to_merge"
    } else if saw_no_actionable {
        "no_actionable_feedback"
    } else {
        return None;
    };
    let mut payload = json!({
        "feedback_category": feedback_category,
        "signals": signals,
    });
    if !pr_numbers.is_empty() {
        if let Some(object) = payload.as_object_mut() {
            object.insert("pr_numbers".to_string(), json!(pr_numbers));
        }
    }
    Some(common_payload(instance, result, decision, payload))
}

fn feedback_category_for_signal(signal: &ActivitySignal) -> Option<&'static str> {
    match signal.signal_type.as_str() {
        "FeedbackFound" | "ChangesRequested" | "ChecksFailed" => Some("blocking_feedback"),
        "PrReadyToMerge" => Some("ready_to_merge"),
        "NoFeedbackFound" => Some("no_actionable_feedback"),
        _ => None,
    }
}

fn common_payload(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    decision: &WorkflowDecisionRecord,
    detail: Value,
) -> Value {
    json!({
        "workflow_id": instance.id,
        "workflow_definition_id": instance.definition_id,
        "terminal_state": instance.state,
        "activity": result.activity,
        "decision": decision.decision.decision,
        "detail": detail,
    })
}

fn clean_json_string(value: Option<&Value>) -> Option<String> {
    clean_string(value?.as_str()?)
}

fn clean_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{
        ActivitySignal, RuntimeKind, WorkflowCommand, WorkflowDecision, WorkflowSubject,
    };
    use crate::runtime::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
    use chrono::{Duration, Utc};
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use uuid::Uuid;

    fn test_prompt_workflow_instance(
        id: &str,
        state: &str,
        repo: Option<&str>,
    ) -> WorkflowInstance {
        let mut data = json!({ "task_id": id });
        if let Some(repo) = repo {
            data["repo"] = json!(repo);
        }
        WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("prompt_task", id),
        )
        .with_id(id)
        .with_data(data)
    }

    fn completion_event(workflow_id: &str, result: ActivityResult) -> WorkflowEvent {
        WorkflowEvent::new(
            workflow_id,
            1,
            crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-1",
        )
        .with_payload(json!({
            "command_id": "command-1",
            "runtime_job_id": "job-1",
            "activity_result": result,
        }))
    }

    fn accepted_decision(workflow_id: &str, next_state: &str) -> WorkflowDecisionRecord {
        WorkflowDecisionRecord::accepted(
            WorkflowDecision::new(
                workflow_id,
                "implementing",
                "finish_prompt_task",
                next_state,
                "runtime completed",
            ),
            Some("event-1".to_string()),
        )
    }

    #[test]
    fn memory_extract_records_done_validation_commands() -> anyhow::Result<()> {
        let instance = test_prompt_workflow_instance("prompt-1", "done", Some("owner/repo"));
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        )
        .with_validation(ValidationRecord::new(
            "cargo test -p harness-workflow memory_extract",
            "passed",
        ))
        .with_validation(ValidationRecord::new("cargo clippy", "skipped"));
        let event = completion_event(&instance.id, result);
        let decision = accepted_decision(&instance.id, "done");

        let record = extract_terminal_repo_memory_record(&instance, &event, &decision)?
            .expect("done validation should produce memory");

        assert_eq!(record.repo, "owner/repo");
        assert_eq!(record.activity_class, PROMPT_TASK_IMPLEMENT_ACTIVITY);
        assert_eq!(record.outcome, RepoMemoryOutcome::Done);
        assert_eq!(record.kind, RepoMemoryKind::ValidationCommand);
        let validation = record.payload_json["detail"]["validation"]
            .as_array()
            .expect("validation detail should be an array");
        assert_eq!(validation.len(), 1);
        assert_eq!(
            validation[0]["command"],
            "cargo test -p harness-workflow memory_extract"
        );
        let expected_evidence_ref = format!("workflow:{}:event:{}", instance.id, event.id);
        assert_eq!(
            record.evidence_ref.as_deref(),
            Some(expected_evidence_ref.as_str())
        );
        Ok(())
    }

    #[test]
    fn memory_extract_records_failed_outcome_as_failure_lesson() -> anyhow::Result<()> {
        let instance = test_prompt_workflow_instance("prompt-2", "failed", Some("owner/repo"));
        let result = ActivityResult::failed(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation failed.",
            "process timed out",
        )
        .with_error_kind(ActivityErrorKind::Timeout)
        .with_validation(ValidationRecord::new("cargo test", "failed"));
        let event = completion_event(&instance.id, result);
        let decision = accepted_decision(&instance.id, "failed");

        let record = extract_terminal_repo_memory_record(&instance, &event, &decision)?
            .expect("failed completion should produce memory");

        assert_eq!(record.outcome, RepoMemoryOutcome::Failed);
        assert_eq!(record.kind, RepoMemoryKind::FailureLesson);
        assert_eq!(record.payload_json["detail"]["failure_class"], "timeout");
        assert_eq!(record.payload_json["detail"]["activity_status"], "failed");
        assert_eq!(record.payload_json["detail"]["has_error_message"], true);
        Ok(())
    }

    #[test]
    fn memory_extract_records_pr_feedback_category_when_no_validation_exists() -> anyhow::Result<()>
    {
        let instance = test_prompt_workflow_instance("prompt-3", "done", Some("owner/repo"));
        let result =
            ActivityResult::succeeded("inspect_pr_feedback", "PR feedback inspection completed.")
                .with_signal(ActivitySignal::new(
                    "PrReadyToMerge",
                    json!({ "repo": "owner/repo", "pr_number": 42 }),
                ));
        let event = completion_event(&instance.id, result);
        let decision = accepted_decision(&instance.id, "done");

        let record = extract_terminal_repo_memory_record(&instance, &event, &decision)?
            .expect("feedback signal should produce memory");

        assert_eq!(record.kind, RepoMemoryKind::ReviewerPattern);
        assert_eq!(
            record.payload_json["detail"]["feedback_category"],
            "ready_to_merge"
        );
        assert_eq!(
            record.payload_json["detail"]["signals"],
            json!(["PrReadyToMerge"])
        );
        assert_eq!(record.payload_json["detail"]["pr_numbers"], json!([42]));
        Ok(())
    }

    #[test]
    fn memory_extract_skips_terminal_runs_without_repo_scope() -> anyhow::Result<()> {
        let instance = test_prompt_workflow_instance("prompt-4", "done", None);
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        )
        .with_validation(ValidationRecord::new("cargo test", "passed"));
        let event = completion_event(&instance.id, result);
        let decision = accepted_decision(&instance.id, "done");

        assert!(extract_terminal_repo_memory_record(&instance, &event, &decision)?.is_none());
        Ok(())
    }

    async fn open_memory_extract_test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(None);
        };
        let schema = format!("memory_extract_{}", Uuid::new_v4().simple());
        let store =
            WorkflowRuntimeStore::open_with_database_url_and_schema(Some(&database_url), &schema)
                .await?;
        Ok(Some(store))
    }

    async fn commit_prompt_completion(
        store: &WorkflowRuntimeStore,
        workflow_id: &str,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<crate::runtime::store::RuntimeActivityCompletion>> {
        let instance =
            test_prompt_workflow_instance(workflow_id, "implementing", Some("owner/repo"));
        store.insert_instance_if_absent(&instance).await?;
        let command = WorkflowCommand::enqueue_activity(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            format!("{workflow_id}:implement"),
        );
        let command_id = store.enqueue_command(&instance.id, None, &command).await?;
        store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({ "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY }),
            )
            .await?;
        let lease_expires_at = Utc::now() + Duration::minutes(5);
        let claimed = store
            .claim_next_runtime_job("runtime-1", lease_expires_at)
            .await?
            .expect("runtime job should be claimable");
        store
            .commit_runtime_activity_completion_if_owned(
                &claimed.id,
                "runtime-1",
                lease_expires_at,
                result,
            )
            .await
    }

    #[tokio::test]
    async fn memory_extract_commit_path_writes_repo_memory_after_terminal_completion(
    ) -> anyhow::Result<()> {
        let Some(store) = open_memory_extract_test_store().await? else {
            return Ok(());
        };
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        )
        .with_validation(ValidationRecord::new(
            "cargo test -p harness-workflow memory_extract",
            "passed",
        ));

        let completion = commit_prompt_completion(&store, "prompt-commit-memory", &result)
            .await?
            .expect("runtime completion should be acknowledged");

        assert!(completion
            .decision
            .as_ref()
            .is_some_and(|record| record.accepted));
        let records = store.list_repo_memory_records("owner/repo").await?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, RepoMemoryKind::ValidationCommand);
        assert_eq!(records[0].outcome, RepoMemoryOutcome::Done);
        Ok(())
    }

    #[tokio::test]
    async fn memory_extract_insert_errors_do_not_block_terminal_completion() -> anyhow::Result<()> {
        let Some(store) = open_memory_extract_test_store().await? else {
            return Ok(());
        };
        sqlx::query("DROP TABLE workflow_repo_memory")
            .execute(store.pool())
            .await?;
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        )
        .with_validation(ValidationRecord::new(
            "cargo test -p harness-workflow memory_extract",
            "passed",
        ));

        let completion = commit_prompt_completion(&store, "prompt-memory-insert-error", &result)
            .await?
            .expect("runtime completion should be acknowledged");

        assert!(completion
            .decision
            .as_ref()
            .is_some_and(|record| record.accepted));
        let instance = store
            .get_instance("prompt-memory-insert-error")
            .await?
            .expect("workflow instance should remain persisted");
        assert_eq!(instance.state, "done");
        Ok(())
    }
}
