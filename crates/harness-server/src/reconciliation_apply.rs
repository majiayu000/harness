use super::*;
use harness_workflow::issue_lifecycle::IssueLifecycleState;
use harness_workflow::runtime::{DataProvenance, WorkflowDataWrite};

pub(super) async fn apply_runtime_workflow_transition(
    runtime_store: &WorkflowRuntimeStore,
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) -> anyhow::Result<bool> {
    let Some(instance) = runtime_store.get_instance(&candidate.workflow_id).await? else {
        return Ok(false);
    };
    if instance.is_terminal() || instance.state != candidate.state {
        return Ok(false);
    }
    apply_loaded_runtime_workflow_transition(
        runtime_store,
        issue_workflows,
        candidate,
        instance,
        target_state,
        reason,
    )
    .await
}

pub(super) async fn apply_loaded_runtime_workflow_transition(
    runtime_store: &WorkflowRuntimeStore,
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    mut instance: WorkflowInstance,
    target_state: &str,
    reason: &str,
) -> anyhow::Result<bool> {
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
    ));
    // Reconciliation reaches `done` from the server's own GitHub observation
    // — a merged PR or a completed issue — never from an agent claim. That
    // observation is exactly the server-recognized terminal proof the
    // transition contract requires (GH-1766).
    let decision = if target_state == "done" {
        decision.with_evidence(WorkflowEvidence::new(
            harness_workflow::runtime::completion_evidence::EVIDENCE_GITHUB_TERMINAL,
            runtime_remote_evidence_summary(candidate),
        ))
    } else {
        decision
    }
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
    apply_runtime_reconciliation_data(
        &mut instance,
        decision_name,
        target_state,
        reason,
        candidate,
    )?;
    let record = runtime_store
        .apply_decision_transition(
            WorkflowDecisionTransition {
                expected_state: candidate.state.as_str(),
                create_if_missing: None,
                event_type,
                source: "reconciliation",
                payload: event_payload,
                decision: &decision,
                final_instance: &instance,
                command_status: WorkflowCommandStatus::Completed,
            },
            "reconciliation",
        )
        .await?;
    if !complete_runtime_workflow_transition(
        record,
        issue_workflows,
        candidate,
        target_state,
        reason,
    )
    .await
    {
        return Ok(false);
    }
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

pub(super) async fn complete_runtime_workflow_transition(
    record: Option<harness_workflow::runtime::WorkflowDecisionRecord>,
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) -> bool {
    let Some(record) = record else {
        return false;
    };
    if !record.accepted {
        tracing::warn!(
            workflow_id = %candidate.workflow_id,
            reason = record.rejection_reason.as_deref().unwrap_or("unspecified validator rejection"),
            "workflow runtime reconciliation transition rejected during atomic validation"
        );
        return false;
    }
    record_runtime_issue_side_effects(issue_workflows, candidate, target_state, reason).await;
    true
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

fn apply_runtime_reconciliation_data(
    instance: &mut WorkflowInstance,
    decision: &str,
    target_state: &str,
    reason: &str,
    candidate: &RuntimeWorkflowCandidate,
) -> anyhow::Result<()> {
    let external_state_field = if candidate.pr_number.is_some() {
        "external_pr_state"
    } else {
        "external_issue_state"
    };
    let mut writes = vec![
        WorkflowDataWrite::set("last_decision", json!(decision), DataProvenance::Server),
        WorkflowDataWrite::set(
            "reconciled_at",
            json!(chrono::Utc::now()),
            DataProvenance::Server,
        ),
        WorkflowDataWrite::set(
            "reconciliation_reason",
            json!(reason),
            DataProvenance::External,
        ),
        WorkflowDataWrite::set(
            external_state_field,
            json!(target_state),
            DataProvenance::External,
        ),
    ];
    if let Some(pr_number) = candidate.pr_number {
        writes.push(WorkflowDataWrite::set(
            "pr_number",
            json!(pr_number),
            DataProvenance::External,
        ));
    }
    if let Some(pr_url) = candidate.pr_url.as_deref() {
        writes.push(WorkflowDataWrite::set(
            "pr_url",
            json!(pr_url),
            DataProvenance::External,
        ));
    }
    if let Some(repo) = candidate.repo.as_deref() {
        writes.push(WorkflowDataWrite::set(
            "repo",
            json!(repo),
            DataProvenance::Server,
        ));
    }
    if let Some(issue_number) = candidate.issue_number {
        writes.push(WorkflowDataWrite::set(
            "issue_number",
            json!(issue_number),
            DataProvenance::Server,
        ));
    }
    instance.apply_data_writes(writes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::WorkflowSubject;

    #[test]
    fn reconciliation_preserves_untouched_field_provenance() {
        let mut instance = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_data_field_provenance(
            json!({
                "agent_note": "agent-authored",
                "external_note": "remote-authored",
                "legacy_note": "pre-migration",
                "server_note": "server-authored",
            }),
            |field| match field {
                "agent_note" => DataProvenance::Agent,
                "external_note" => DataProvenance::External,
                _ => DataProvenance::Server,
            },
        );
        let provenance = instance
            .data_provenance
            .as_mut()
            .expect("classified instance has provenance");
        provenance.entries.remove("/legacy_note");
        provenance.legacy_entries.insert("/legacy_note".to_string());
        let candidate = RuntimeWorkflowCandidate {
            workflow_id: instance.id.clone(),
            state: instance.state.clone(),
            row_updated_at: chrono::Utc::now(),
            repo: Some("owner/repo".to_string()),
            project_root: None,
            issue_number: Some(42),
            pr_number: None,
            pr_url: None,
        };

        apply_runtime_reconciliation_data(
            &mut instance,
            "reconcile_issue_completed",
            "done",
            "remote issue is closed",
            &candidate,
        )
        .expect("reconciliation write");

        let provenance = instance
            .data_provenance
            .as_ref()
            .expect("reconciliation preserves provenance");
        assert_eq!(
            provenance.provenance_for("/agent_note"),
            Some(DataProvenance::Agent)
        );
        assert_eq!(
            provenance.provenance_for("/external_note"),
            Some(DataProvenance::External)
        );
        assert!(provenance.is_legacy("/legacy_note"));
        assert_eq!(
            provenance.provenance_for("/server_note"),
            Some(DataProvenance::Server)
        );
        assert_eq!(
            provenance.provenance_for("/external_issue_state"),
            Some(DataProvenance::External)
        );
        assert_eq!(
            provenance.provenance_for("/last_decision"),
            Some(DataProvenance::Server)
        );
    }
}
