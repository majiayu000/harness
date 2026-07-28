use crate::runtime::WorkflowDecisionRecord;

pub(super) fn accepted_transition_record(
    record: Option<WorkflowDecisionRecord>,
    workflow_id: &str,
    operation: &str,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    match record {
        Some(record) if record.accepted => Ok(Some(record)),
        Some(record) => {
            let reason = record
                .rejection_reason
                .as_deref()
                .unwrap_or("unspecified validator rejection");
            anyhow::bail!("{operation} for workflow `{workflow_id}` was rejected: {reason}");
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::WorkflowDecision;

    fn eval_decision() -> WorkflowDecision {
        WorkflowDecision::new(
            "eval-transition-outcome",
            "implementing",
            "cancel_eval_run",
            "cancelled",
            "operator cancelled eval run",
        )
    }

    #[test]
    fn accepted_transition_record_distinguishes_all_atomic_outcomes() {
        let accepted = WorkflowDecisionRecord::accepted(eval_decision(), None);
        assert!(
            accepted_transition_record(Some(accepted), "workflow-1", "eval cleanup")
                .expect("accepted transition should succeed")
                .is_some()
        );
        assert!(
            accepted_transition_record(None, "workflow-1", "eval cleanup")
                .expect("stale transition should remain distinguishable")
                .is_none()
        );

        let rejected =
            WorkflowDecisionRecord::rejected(eval_decision(), None, "lease expired before commit");
        let error = accepted_transition_record(Some(rejected), "workflow-1", "eval cleanup")
            .expect_err("rejected transition must fail");
        assert_eq!(
            error.to_string(),
            "eval cleanup for workflow `workflow-1` was rejected: lease expired before commit"
        );
    }
}
