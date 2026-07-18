use crate::http::AppState;
use chrono::{DateTime, Utc};
use harness_workflow::runtime::WorkflowSubmissionMetrics;

pub(super) async fn load_submission_metrics(
    state: &AppState,
    since: DateTime<Utc>,
) -> anyhow::Result<WorkflowSubmissionMetrics> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(WorkflowSubmissionMetrics::default());
    };
    store.submission_metrics_since(since).await
}

pub(super) async fn load_runtime_turn_counts(state: &AppState) -> anyhow::Result<Vec<u32>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.runtime_turn_counts().await
}
