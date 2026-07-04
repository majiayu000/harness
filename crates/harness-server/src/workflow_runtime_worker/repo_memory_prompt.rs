use harness_workflow::runtime::{
    ActivityArtifact, RepoMemoryRetrievalOptions, RetrievedRepoMemoryRecord, RuntimeJob,
    WorkflowInstance, WorkflowRuntimeStore, REPO_MEMORY_CONFIG_ARTIFACT,
    REPO_MEMORY_DEGRADATION_ARTIFACT,
};
use serde_json::{json, Value};

use super::data_helpers::activity_name;

#[derive(Debug, Default)]
pub(super) struct RepoMemoryPromptContext {
    pub(super) records: Vec<RetrievedRepoMemoryRecord>,
    pub(super) degradation: Option<ActivityArtifact>,
}

pub(super) async fn repo_memory_for_prompt_packet(
    memory_enabled: bool,
    store: Option<&WorkflowRuntimeStore>,
    workflow: Option<&WorkflowInstance>,
    job: &RuntimeJob,
) -> RepoMemoryPromptContext {
    if !memory_enabled {
        return RepoMemoryPromptContext::default();
    }
    let Some(repo) = repo_for_memory_prompt(workflow, job) else {
        return RepoMemoryPromptContext::default();
    };
    let activity = activity_name(job);
    let Some(store) = store else {
        return RepoMemoryPromptContext {
            records: Vec::new(),
            degradation: Some(repo_memory_degradation_artifact(
                job,
                &repo,
                &activity,
                "workflow_runtime_store_unavailable",
                None,
            )),
        };
    };
    match store
        .retrieve_repo_memory_records(&repo, &activity, RepoMemoryRetrievalOptions::default())
        .await
    {
        Ok(records) => RepoMemoryPromptContext {
            records,
            degradation: None,
        },
        Err(error) => {
            tracing::warn!(
                runtime_job_id = %job.id,
                repo = %repo,
                activity = %activity,
                "failed to retrieve repo memory for runtime prompt packet: {error}"
            );
            RepoMemoryPromptContext {
                records: Vec::new(),
                degradation: Some(repo_memory_degradation_artifact(
                    job,
                    &repo,
                    &activity,
                    "repo_memory_retrieval_failed",
                    Some(&error),
                )),
            }
        }
    }
}

pub(super) fn repo_memory_config_artifact(enabled: bool) -> ActivityArtifact {
    ActivityArtifact::new(
        REPO_MEMORY_CONFIG_ARTIFACT,
        json!({
            "schema": "harness.runtime.repo_memory_config.v1",
            "enabled": enabled,
        }),
    )
}

fn repo_memory_degradation_artifact(
    job: &RuntimeJob,
    repo: &str,
    activity: &str,
    reason: &str,
    error: Option<&anyhow::Error>,
) -> ActivityArtifact {
    let mut artifact = json!({
        "schema": "harness.runtime.repo_memory_degradation.v1",
        "memory_enabled": true,
        "reason": reason,
        "repo": repo,
        "activity": activity,
        "runtime_job_id": job.id,
    });
    if let Some(error) = error {
        artifact["error"] = json!(error.to_string());
    }
    ActivityArtifact::new(REPO_MEMORY_DEGRADATION_ARTIFACT, artifact)
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeJob, RuntimeKind};

    fn repo_runtime_job() -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "implement_issue",
                "repo": "owner/repo",
            }),
        )
    }

    #[tokio::test]
    async fn memory_flag_degraded_disabled_flag_skips_prompt_memory_lookup() {
        let context = repo_memory_for_prompt_packet(false, None, None, &repo_runtime_job()).await;

        assert!(context.records.is_empty());
        assert!(context.degradation.is_none());
    }

    #[tokio::test]
    async fn memory_flag_degraded_enabled_store_unavailable_records_degradation() {
        let job = repo_runtime_job();
        let context = repo_memory_for_prompt_packet(true, None, None, &job).await;

        assert!(context.records.is_empty());
        let Some(degradation) = context.degradation else {
            panic!("enabled memory without a store should produce visible evidence");
        };
        assert_eq!(degradation.artifact_type, REPO_MEMORY_DEGRADATION_ARTIFACT);
        assert_eq!(
            degradation.artifact["reason"],
            "workflow_runtime_store_unavailable"
        );
        assert_eq!(degradation.artifact["repo"], "owner/repo");
        assert_eq!(degradation.artifact["activity"], "implement_issue");
        assert_eq!(degradation.artifact["memory_enabled"], true);
    }
}
