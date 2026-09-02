use super::*;

pub(super) enum HygieneRepairConvergence {
    Continue { next_round: u64 },
    Stop(PrFeedbackSweepRequestOutcome),
}

pub(super) async fn evaluate_hygiene_repair_convergence(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    new_instance: bool,
    issue_number: Option<u64>,
    ctx: &PrHygieneRepairRuntimeContext<'_>,
) -> anyhow::Result<HygieneRepairConvergence> {
    let current_blockers = 1;
    let stop = match next_feedback_repair_round(
        &instance.data,
        current_blockers,
        FeedbackRepairLane::RemoteFeedback,
    ) {
        Ok(next_round) => return Ok(HygieneRepairConvergence::Continue { next_round }),
        Err(stop) => stop,
    };
    let (decision_name, reason) = hygiene_repair_stop(stop);
    let workflow_id = instance.id.clone();
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "blocked",
        &reason,
    )
    .with_command(hygiene_convergence_blocked_command(&instance.id, &reason))
    .with_evidence(WorkflowEvidence::new("pr_feedback_convergence", &reason))
    .high_confidence();
    let outcome = commit_runtime_decision(
        store,
        instance.clone(),
        new_instance,
        decision,
        "PrHygieneRepairBlocked",
        "workflow_runtime_pr_hygiene",
        json!({
            "issue_number": issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
        }),
        instance.data.clone(),
    )
    .await?;
    let outcome = match outcome {
        RuntimeDecisionCommitOutcome::Accepted => PrFeedbackSweepRequestOutcome::Rejected {
            workflow_id,
            reason,
        },
        RuntimeDecisionCommitOutcome::Rejected { reason } => {
            PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id,
                reason,
            }
        }
        RuntimeDecisionCommitOutcome::Stale => PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id,
            state: "stale".to_string(),
        },
    };
    Ok(HygieneRepairConvergence::Stop(outcome))
}

fn hygiene_convergence_blocked_command(instance_id: &str, reason: &str) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::MarkBlocked,
        format!("pr-hygiene:{instance_id}:convergence-block"),
        json!({
            "reason": reason,
            "last_stop": {
                "state": "blocked",
                "activity": "address_pr_feedback",
                "source": PR_HYGIENE_CONVERGENCE_STOP_SOURCE,
            },
        }),
    )
}

fn hygiene_repair_stop(stop: FeedbackRepairStop) -> (&'static str, String) {
    match stop {
        FeedbackRepairStop::RoundLimit { completed_rounds } => (
            "block_feedback_repair_round_limit",
            format!(
                "PR hygiene repair remains actionable after {completed_rounds} repair rounds; operator review is required before more mutations."
            ),
        ),
        FeedbackRepairStop::MissingBaseline { completed_rounds } => (
            "block_feedback_repair_unmeasured",
            format!(
                "PR hygiene repair progress cannot be measured after {completed_rounds} repair rounds because the prior blocker baseline is missing; automatic repair is stopped."
            ),
        ),
        FeedbackRepairStop::NoProgress { previous, current } => (
            "block_feedback_repair_oscillation",
            format!(
                "PR hygiene repair did not decrease actionable blockers ({previous} before, {current} now); automatic repair is stopped to prevent oscillation."
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convergence_block_records_hygiene_recovery_source() {
        let command = hygiene_convergence_blocked_command("workflow-1", "no progress");

        assert_eq!(command.command_type, WorkflowCommandType::MarkBlocked);
        assert_eq!(command.command["last_stop"]["state"], "blocked");
        assert_eq!(
            command.command["last_stop"]["activity"],
            "address_pr_feedback"
        );
        assert_eq!(
            command.command["last_stop"]["source"],
            PR_HYGIENE_CONVERGENCE_STOP_SOURCE
        );
    }
}
