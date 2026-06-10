use super::IssueWorkflowStore;
use crate::issue_lifecycle::{
    IssueLifecycleEvent, IssueLifecycleEventKind, IssueLifecycleState, IssueWorkflowInstance,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueMergeApprovalOutcome {
    Applied(IssueWorkflowInstance),
    IgnoredWrongState {
        actual: IssueLifecycleState,
        workflow: IssueWorkflowInstance,
    },
    NotFound,
}

impl IssueWorkflowStore {
    /// Transition a `ReadyToMerge` workflow to `Done` after human approval.
    pub async fn record_merge_approved(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> anyhow::Result<IssueMergeApprovalOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some((wf_id, mut workflow)) = self
            .load_for_update_by_pr(&mut tx, project_id, repo, pr_number)
            .await?
        else {
            return Ok(IssueMergeApprovalOutcome::NotFound);
        };

        if workflow.state != IssueLifecycleState::ReadyToMerge {
            let actual = workflow.state;
            return Ok(IssueMergeApprovalOutcome::IgnoredWrongState { actual, workflow });
        }

        workflow.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ));
        self.upsert_in_tx(&mut tx, &workflow).await?;
        debug_assert_eq!(workflow.id, wf_id);
        tx.commit().await?;
        Ok(IssueMergeApprovalOutcome::Applied(workflow))
    }
}
