use harness_workflow::runtime::{
    RepoMemoryRetrievalOptions, RetrievedRepoMemoryRecord, RuntimeJob, WorkflowInstance,
    WorkflowRuntimeStore,
};
use serde_json::Value;

use super::data_helpers::activity_name;

pub(super) async fn repo_memory_for_prompt_packet(
    store: Option<&WorkflowRuntimeStore>,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> Vec<RetrievedRepoMemoryRecord> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Some(repo) = repo_for_memory_prompt(workflow, job) else {
        return Vec::new();
    };
    let activity = activity_name(job);
    match store
        .retrieve_repo_memory_records(&repo, &activity, RepoMemoryRetrievalOptions::default())
        .await
    {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(
                runtime_job_id = %job.id,
                repo = %repo,
                activity = %activity,
                "failed to retrieve repo memory for runtime prompt packet: {error}"
            );
            Vec::new()
        }
    }
}

fn repo_for_memory_prompt(workflow: Option<&WorkflowInstance>, job: &RuntimeJob) -> Option<String> {
    workflow
        .and_then(|workflow| workflow.data.get("repo"))
        .and_then(Value::as_str)
        .or_else(|| job.input.get("repo").and_then(Value::as_str))
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
        .map(str::to_string)
}
