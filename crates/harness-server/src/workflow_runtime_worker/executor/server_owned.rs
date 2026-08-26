use crate::http::AppState;
use harness_workflow::runtime::{ActivityResult, RuntimeJob, WorkflowInstance};
use std::sync::Arc;

pub(super) async fn execute(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<ActivityResult>> {
    match super::super::data_helpers::activity_name(job).as_str() {
        "start_child_workflow" => Ok(Some(
            super::super::child_workflow::execute_start_child_workflow(state, job, parent).await?,
        )),
        activity if activity == harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY => {
            Ok(Some(
                super::super::pr_feedback_inspection::execute_pr_feedback_inspection(
                    state, job, parent,
                )
                .await,
            ))
        }
        "merge_pr"
            if super::super::server_merge::server_merge_execution_enabled(state, job, parent) =>
        {
            Ok(Some(
                super::super::server_merge::execute_server_merge(state, job, parent).await,
            ))
        }
        _ => Ok(None),
    }
}
