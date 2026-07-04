use super::{
    depends_on_strings, insert_author_trust_class, merge_last_decision, optional_string_field,
    string_field, IssueSubmissionRuntimeContext, PromptSubmissionRuntimeContext,
    WorkflowSubmissionRuntimeRecord, EXECUTION_PATH_WORKFLOW_RUNTIME,
};
use super::{
    prompt_memory::{cache_prompt_submission_prompt, remove_prompt_submission_prompt},
    replay::{
        decision_for_event, disambiguate_submission_command_dedupe, submission_event_for_replay,
        SubmissionEventSelection,
    },
};
use harness_workflow::runtime::{
    DecisionValidator, ValidationContext, WorkflowCommandStatus, WorkflowDecision,
    WorkflowInstance, WorkflowRuntimeStore, WorkflowSubmissionDecisionTransition,
    WorkflowSubmissionPromptPayload,
};
use serde_json::json;

pub(super) async fn apply_decision(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    mut decision: WorkflowDecision,
    ctx: &IssueSubmissionRuntimeContext<'_>,
    accepted_data: serde_json::Value,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let validation_context = if instance.is_terminal() {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    let event = issue_submission_event_for_commit(store, &instance.id, ctx).await?;
    let new_event_id = new_submission_event_id(&event);
    if event.disambiguates_command_dedupe() {
        let event_id = new_event_id
            .as_deref()
            .expect("new replay attempt should reserve an event id");
        disambiguate_submission_command_dedupe(&mut decision, event_id);
    }
    let existing_record = match event.event_id.as_deref() {
        Some(event_id) => decision_for_event(store, &instance.id, event_id).await?,
        None => None,
    };
    if let Some(record) = existing_record.as_ref().filter(|record| !record.accepted) {
        return Ok(WorkflowSubmissionRuntimeRecord {
            workflow_id: instance.id,
            accepted: false,
            decision_id: record.id.clone(),
            command_ids: Vec::new(),
            rejection_reason: record.rejection_reason.clone(),
        });
    }

    if existing_record.is_none() {
        if let Err(error) =
            DecisionValidator::github_issue_pr().validate(&instance, &decision, &validation_context)
        {
            let reason = error.to_string();
            let rejected_instance = new_instance
                .then(|| rejected_submission_instance(instance.clone(), accepted_data, &reason));
            let outcome = store
                .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                    workflow_id: &instance.id,
                    expected_state: &instance.state,
                    expected_version: instance.version,
                    create_if_missing: new_instance.then_some(&instance),
                    event_id: event.event_id.as_deref(),
                    new_event_id: new_event_id.as_deref(),
                    event_type: "IssueSubmitted",
                    source: "workflow_runtime_submission",
                    payload: issue_submission_event_payload(ctx),
                    decision: &decision,
                    existing_record: None,
                    rejection_reason: Some(&reason),
                    final_instance: rejected_instance.as_ref(),
                    command_status: WorkflowCommandStatus::Pending,
                    prompt_payload: None,
                })
                .await?
                .ok_or_else(|| submission_commit_conflict(&instance.id))?;
            return Ok(WorkflowSubmissionRuntimeRecord {
                workflow_id: instance.id,
                accepted: false,
                decision_id: outcome.record.id,
                command_ids: Vec::new(),
                rejection_reason: Some(reason),
            });
        }
    }

    let committed_decision = existing_record
        .as_ref()
        .map(|record| &record.decision)
        .unwrap_or(&decision);
    let final_instance = accepted_submission_instance(
        instance.clone(),
        committed_decision,
        accepted_data,
        preserves_applied_instance(&instance, existing_record.as_ref()),
    );
    let outcome = store
        .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
            workflow_id: &instance.id,
            expected_state: &instance.state,
            expected_version: instance.version,
            create_if_missing: new_instance.then_some(&instance),
            event_id: event.event_id.as_deref(),
            new_event_id: new_event_id.as_deref(),
            event_type: "IssueSubmitted",
            source: "workflow_runtime_submission",
            payload: issue_submission_event_payload(ctx),
            decision: &decision,
            existing_record: existing_record.as_ref(),
            rejection_reason: None,
            final_instance: Some(&final_instance),
            command_status: WorkflowCommandStatus::Pending,
            prompt_payload: None,
        })
        .await?
        .ok_or_else(|| submission_commit_conflict(&instance.id))?;
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: instance.id,
        accepted: true,
        decision_id: outcome.record.id,
        command_ids: outcome.command_ids,
        rejection_reason: None,
    })
}

pub(super) async fn apply_prompt_decision(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    mut decision: WorkflowDecision,
    ctx: &PromptSubmissionRuntimeContext<'_>,
    accepted_data: serde_json::Value,
) -> anyhow::Result<WorkflowSubmissionRuntimeRecord> {
    let validation_context = if instance.is_terminal() {
        ValidationContext::new("workflow-policy", chrono::Utc::now()).allow_terminal_reopen()
    } else {
        ValidationContext::new("workflow-policy", chrono::Utc::now())
    };
    let event = prompt_submission_event_for_commit(store, &instance.id, ctx).await?;
    let new_event_id = new_submission_event_id(&event);
    if event.disambiguates_command_dedupe() {
        let event_id = new_event_id
            .as_deref()
            .expect("new replay attempt should reserve an event id");
        disambiguate_submission_command_dedupe(&mut decision, event_id);
    }
    let existing_record = match event.event_id.as_deref() {
        Some(event_id) => decision_for_event(store, &instance.id, event_id).await?,
        None => None,
    };
    if let Some(record) = existing_record.as_ref().filter(|record| !record.accepted) {
        return Ok(WorkflowSubmissionRuntimeRecord {
            workflow_id: instance.id,
            accepted: false,
            decision_id: record.id.clone(),
            command_ids: Vec::new(),
            rejection_reason: record.rejection_reason.clone(),
        });
    }

    if existing_record.is_none() {
        if let Err(error) =
            DecisionValidator::prompt_task().validate(&instance, &decision, &validation_context)
        {
            let reason = error.to_string();
            let rejected_instance = new_instance
                .then(|| rejected_submission_instance(instance.clone(), accepted_data, &reason));
            let outcome = store
                .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                    workflow_id: &instance.id,
                    expected_state: &instance.state,
                    expected_version: instance.version,
                    create_if_missing: new_instance.then_some(&instance),
                    event_id: event.event_id.as_deref(),
                    new_event_id: new_event_id.as_deref(),
                    event_type: "PromptSubmitted",
                    source: "workflow_runtime_submission",
                    payload: prompt_submission_event_payload(ctx),
                    decision: &decision,
                    existing_record: None,
                    rejection_reason: Some(&reason),
                    final_instance: rejected_instance.as_ref(),
                    command_status: WorkflowCommandStatus::Pending,
                    prompt_payload: None,
                })
                .await?
                .ok_or_else(|| submission_commit_conflict(&instance.id))?;
            return Ok(WorkflowSubmissionRuntimeRecord {
                workflow_id: instance.id,
                accepted: false,
                decision_id: outcome.record.id,
                command_ids: Vec::new(),
                rejection_reason: Some(reason),
            });
        }
    }

    let prompt_ref = string_field(&accepted_data, "prompt_ref")?;
    let committed_decision = existing_record
        .as_ref()
        .map(|record| &record.decision)
        .unwrap_or(&decision);
    let final_instance = accepted_submission_instance(
        instance.clone(),
        committed_decision,
        accepted_data,
        preserves_applied_instance(&instance, existing_record.as_ref()),
    );
    let final_prompt_ref = optional_string_field(&final_instance.data, "prompt_ref");
    let prompt_payload_commits = final_prompt_ref.as_deref() == Some(prompt_ref.as_str());
    let previous_prompt_ref = optional_string_field(&instance.data, "prompt_ref");
    let previous_prompt_ref_to_remove = prompt_payload_commits
        .then_some(previous_prompt_ref.as_deref())
        .flatten()
        .filter(|previous_prompt_ref| *previous_prompt_ref != prompt_ref.as_str());
    let outcome = store
        .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
            workflow_id: &instance.id,
            expected_state: &instance.state,
            expected_version: instance.version,
            create_if_missing: new_instance.then_some(&instance),
            event_id: event.event_id.as_deref(),
            new_event_id: new_event_id.as_deref(),
            event_type: "PromptSubmitted",
            source: "workflow_runtime_submission",
            payload: prompt_submission_event_payload(ctx),
            decision: &decision,
            existing_record: existing_record.as_ref(),
            rejection_reason: None,
            final_instance: Some(&final_instance),
            command_status: WorkflowCommandStatus::Pending,
            prompt_payload: prompt_payload_commits.then_some(WorkflowSubmissionPromptPayload {
                prompt_ref: &prompt_ref,
                prompt: ctx.prompt,
                previous_prompt_ref: previous_prompt_ref_to_remove,
            }),
        })
        .await?
        .ok_or_else(|| submission_commit_conflict(&instance.id))?;
    if prompt_payload_commits {
        cache_prompt_submission_prompt(&prompt_ref, ctx.prompt);
        remove_prompt_submission_prompt(previous_prompt_ref_to_remove);
    }
    Ok(WorkflowSubmissionRuntimeRecord {
        workflow_id: instance.id,
        accepted: true,
        decision_id: outcome.record.id,
        command_ids: outcome.command_ids,
        rejection_reason: None,
    })
}

async fn issue_submission_event_for_commit(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    ctx: &IssueSubmissionRuntimeContext<'_>,
) -> anyhow::Result<SubmissionEventSelection> {
    let lookup =
        submission_event_for_replay(store, workflow_id, "IssueSubmitted", ctx.task_id).await?;
    Ok(SubmissionEventSelection {
        event_id: lookup.event_id,
        has_prior_attempt: lookup.has_prior_attempt,
    })
}

async fn prompt_submission_event_for_commit(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    ctx: &PromptSubmissionRuntimeContext<'_>,
) -> anyhow::Result<SubmissionEventSelection> {
    let lookup =
        submission_event_for_replay(store, workflow_id, "PromptSubmitted", ctx.task_id).await?;
    Ok(SubmissionEventSelection {
        event_id: lookup.event_id,
        has_prior_attempt: lookup.has_prior_attempt,
    })
}

fn new_submission_event_id(event: &SubmissionEventSelection) -> Option<String> {
    event
        .disambiguates_command_dedupe()
        .then(|| uuid::Uuid::new_v4().to_string())
}

fn issue_submission_event_payload(ctx: &IssueSubmissionRuntimeContext<'_>) -> serde_json::Value {
    let mut payload = json!({
        "task_id": ctx.task_id.as_str(),
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "labels": ctx.labels,
        "force_execute": ctx.force_execute,
        "additional_prompt": ctx.additional_prompt,
        "depends_on": depends_on_strings(ctx.depends_on),
        "dependencies_blocked": ctx.dependencies_blocked,
        "source": ctx.source,
        "external_id": ctx.external_id,
        "remote_fact_hash": ctx.remote_fact_hash,
        "tracker_source": super::issue_tracker_source(ctx),
        "tracker_external_id": super::issue_tracker_external_id(ctx),
        "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
    });
    insert_author_trust_class(&mut payload, ctx.author_trust_class);
    payload
}

fn prompt_submission_event_payload(ctx: &PromptSubmissionRuntimeContext<'_>) -> serde_json::Value {
    json!({
        "task_id": ctx.task_id.as_str(),
        "prompt_chars": ctx.prompt.chars().count(),
        "depends_on": depends_on_strings(ctx.depends_on),
        "serialization_depends_on": depends_on_strings(ctx.serialization_depends_on),
        "dependencies_blocked": ctx.dependencies_blocked,
        "source": ctx.source,
        "external_id": ctx.external_id,
        "execution_path": EXECUTION_PATH_WORKFLOW_RUNTIME,
    })
}

fn accepted_submission_instance(
    mut instance: WorkflowInstance,
    decision: &WorkflowDecision,
    accepted_data: serde_json::Value,
    preserve_current: bool,
) -> WorkflowInstance {
    if preserve_current {
        return instance;
    }
    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_last_decision(accepted_data, &decision.decision);
    instance
}

fn rejected_submission_instance(
    mut instance: WorkflowInstance,
    mut rejected_data: serde_json::Value,
    reason: &str,
) -> WorkflowInstance {
    instance.state = "failed".to_string();
    instance.version = instance.version.saturating_add(1);
    if let Some(data) = rejected_data.as_object_mut() {
        data.insert("rejection_reason".to_string(), json!(reason));
        data.insert(
            "rejected_at".to_string(),
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }
    instance.data = merge_last_decision(rejected_data, "submission_rejected");
    instance
}

fn preserves_applied_instance(
    instance: &WorkflowInstance,
    record: Option<&harness_workflow::runtime::WorkflowDecisionRecord>,
) -> bool {
    record.is_some_and(|record| record.accepted && instance.state == record.decision.next_state)
}

fn submission_commit_conflict(workflow_id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "workflow submission `{workflow_id}` changed state or version before decision commit"
    )
}
