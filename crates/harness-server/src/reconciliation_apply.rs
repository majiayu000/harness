use super::*;
use harness_workflow::issue_lifecycle::IssueLifecycleState;

pub(super) async fn apply_runtime_workflow_transition(
    runtime_store: &WorkflowRuntimeStore,
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) -> anyhow::Result<bool> {
    let Some(mut instance) = runtime_store.get_instance(&candidate.workflow_id).await? else {
        return Ok(false);
    };
    if instance.is_terminal() || instance.state != candidate.state {
        return Ok(false);
    }

    let is_pr_target = candidate.pr_number.is_some();
    let event_type = match (target_state, is_pr_target) {
        ("done", true) => "PrMerged",
        ("done", false) => "IssueCompleted",
        ("cancelled", true) => "PrClosed",
        ("cancelled", false) => "IssueClosed",
        (_, true) => "ExternalPrStateObserved",
        (_, false) => "ExternalIssueStateObserved",
    };
    let decision_name = match (target_state, is_pr_target) {
        ("done", true) => "reconcile_pr_merged",
        ("done", false) => "reconcile_issue_completed",
        ("cancelled", true) => "reconcile_pr_closed",
        ("cancelled", false) => "reconcile_issue_closed",
        (_, true) => "reconcile_pr_state",
        (_, false) => "reconcile_issue_state",
    };
    let command_type = match target_state {
        "done" => WorkflowCommandType::MarkDone,
        "cancelled" => WorkflowCommandType::MarkCancelled,
        _ => WorkflowCommandType::Wait,
    };
    let event_payload = remote_payload(candidate, target_state, reason);
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        target_state,
        reason,
    )
    .with_command(WorkflowCommand::new(
        command_type,
        format!(
            "runtime-reconcile:{}:{}:{}",
            instance.id,
            target_state,
            runtime_remote_key(candidate)
        ),
        remote_payload(candidate, target_state, reason),
    ))
    .with_evidence(WorkflowEvidence::new(
        if is_pr_target {
            "github_pr"
        } else {
            "github_issue"
        },
        runtime_remote_evidence_summary(candidate),
    ))
    .high_confidence();
    let validator = DecisionValidator::github_issue_pr();
    if let Err(error) = validator.validate(
        &instance,
        &decision,
        &ValidationContext::new("reconciliation", chrono::Utc::now()),
    ) {
        let reason = error.to_string();
        tracing::warn!(
            workflow_id = %candidate.workflow_id,
            pr = ?candidate.pr_number,
            issue = ?candidate.issue_number,
            repo = candidate.repo.as_deref(),
            "workflow runtime reconciliation decision rejected: {reason}"
        );
        return Ok(false);
    }

    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_runtime_reconciliation_data(
        instance.data,
        decision_name,
        target_state,
        reason,
        candidate,
    );
    let Some(_record) = runtime_store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: candidate.state.as_str(),
            create_if_missing: None,
            event_type,
            source: "reconciliation",
            payload: event_payload,
            decision: &decision,
            final_instance: &instance,
            command_status: WorkflowCommandStatus::Completed,
        })
        .await?
    else {
        return Ok(false);
    };
    record_runtime_issue_side_effects(issue_workflows, candidate, target_state, reason).await;
    tracing::info!(
        workflow_id = %candidate.workflow_id,
        from = %candidate.state,
        to = target_state,
        pr = ?candidate.pr_number,
        issue = ?candidate.issue_number,
        repo = candidate.repo.as_deref(),
        "workflow runtime reconciliation: applying transition"
    );
    Ok(true)
}

fn remote_payload(
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) -> serde_json::Value {
    json!({
        "repo": candidate.repo.as_deref(),
        "issue_number": candidate.issue_number,
        "pr_number": candidate.pr_number,
        "pr_url": candidate.pr_url.as_deref(),
        "target_state": target_state,
        "reason": reason,
    })
}

fn runtime_remote_key(candidate: &RuntimeWorkflowCandidate) -> String {
    candidate
        .pr_number
        .map(|number| format!("pr-{number}"))
        .or_else(|| {
            candidate
                .issue_number
                .map(|number| format!("issue-{number}"))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn runtime_remote_evidence_summary(candidate: &RuntimeWorkflowCandidate) -> String {
    let repo = candidate.repo.as_deref().unwrap_or("<unknown>");
    let issue = candidate
        .issue_number
        .map(|number| number.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    match candidate.pr_number {
        Some(pr_number) => {
            let url = candidate.pr_url.as_deref().unwrap_or("<unknown>");
            format!("repo={repo} issue={issue} pr={pr_number} url={url}")
        }
        None => format!("repo={repo} issue={issue}"),
    }
}

fn merge_runtime_reconciliation_data(
    mut data: serde_json::Value,
    decision: &str,
    target_state: &str,
    reason: &str,
    candidate: &RuntimeWorkflowCandidate,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert("reconciled_at".to_string(), json!(chrono::Utc::now()));
        object.insert("reconciliation_reason".to_string(), json!(reason));
        let external_state_field = if candidate.pr_number.is_some() {
            "external_pr_state"
        } else {
            "external_issue_state"
        };
        object.insert(external_state_field.to_string(), json!(target_state));
        if let Some(pr_number) = candidate.pr_number {
            object.insert("pr_number".to_string(), json!(pr_number));
        }
        if let Some(pr_url) = candidate.pr_url.as_deref() {
            object.insert("pr_url".to_string(), json!(pr_url));
        }
        if let Some(repo) = candidate.repo.as_deref() {
            object.insert("repo".to_string(), json!(repo));
        }
        if let Some(issue_number) = candidate.issue_number {
            object.insert("issue_number".to_string(), json!(issue_number));
        }
    }
    data
}

async fn record_runtime_issue_side_effects(
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) {
    let (Some(project_root), Some(issue_workflows)) =
        (candidate.project_root.as_deref(), issue_workflows)
    else {
        return;
    };
    let project_id = project_root.to_string_lossy();
    let Some(pr_number) = candidate.pr_number else {
        let Some(issue_number) = candidate.issue_number else {
            return;
        };
        let Some(final_state) = issue_terminal_state(target_state) else {
            return;
        };
        if let Err(error) = issue_workflows
            .record_terminal_for_issue(
                &project_id,
                candidate.repo.as_deref(),
                issue_number,
                final_state,
                Some(reason),
            )
            .await
        {
            tracing::warn!(
                repo = candidate.repo.as_deref().unwrap_or("<unknown>"),
                issue_number,
                "reconciliation: failed to record terminal issue state: {error}"
            );
        }
        return;
    };
    if target_state == "done" {
        let result = if let Some(issue_number) = candidate.issue_number {
            issue_workflows
                .record_pr_merged_for_issue(
                    &project_id,
                    candidate.repo.as_deref(),
                    issue_number,
                    pr_number,
                    candidate.pr_url.as_deref(),
                    Some(reason),
                )
                .await
        } else {
            issue_workflows
                .record_pr_merged(
                    &project_id,
                    candidate.repo.as_deref(),
                    pr_number,
                    Some(reason),
                )
                .await
        };
        if let Err(error) = result {
            tracing::warn!(
                repo = candidate.repo.as_deref().unwrap_or("<unknown>"),
                pr_number,
                "reconciliation: failed to record merged PR in issue workflow store: {error}"
            );
        }
        return;
    }
    if target_state == "cancelled" {
        if let Err(error) = issue_workflows
            .record_terminal_for_pr(
                &project_id,
                candidate.repo.as_deref(),
                pr_number,
                false,
                true,
                Some(reason),
            )
            .await
        {
            tracing::warn!(
                pr = pr_number,
                repo = candidate.repo.as_deref(),
                "issue workflow closed PR update failed: {error}"
            );
        }
    }
}

pub(super) fn issue_terminal_state(target_state: &str) -> Option<IssueLifecycleState> {
    match target_state {
        "done" => Some(IssueLifecycleState::Done),
        "cancelled" => Some(IssueLifecycleState::Cancelled),
        _ => None,
    }
}
