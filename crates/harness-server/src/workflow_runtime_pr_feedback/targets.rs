//! Workflow target loading, instance construction, workflow IDs, runtime JSON
//! construction, and field parsing helpers for the PR feedback runtime.

use super::PrRuntimeTarget;
use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    PrFeedbackOutcome, WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::json;
use std::path::Path;

pub(super) fn runtime_task_id_from_instance(instance: &WorkflowInstance) -> String {
    instance
        .data
        .get("task_id")
        .and_then(|value| value.as_str())
        .filter(|task_id| !task_id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("runtime:{}", instance.id))
}

pub(super) fn pr_lifecycle_failure_instance(
    project_root: &Path,
    repo: Option<&str>,
    issue_number: Option<u64>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
) -> WorkflowInstance {
    let project_id = project_root.to_string_lossy().into_owned();
    let mut instance = if let Some(issue_number) = issue_number {
        let workflow_id =
            harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number);
        issue_instance(
            workflow_id,
            project_id,
            repo.map(ToOwned::to_owned),
            issue_number,
            "failed",
        )
    } else {
        pr_scoped_instance(
            pr_workflow_id(&project_id, repo, pr_number),
            project_id,
            repo.map(ToOwned::to_owned),
            task_id,
            pr_number,
            pr_url,
            "failed",
        )
    };
    if let Some(data) = instance.data.as_object_mut() {
        data.insert("task_id".to_string(), json!(task_id.as_str()));
        data.insert("pr_number".to_string(), json!(pr_number));
        data.insert("pr_url".to_string(), json!(pr_url));
        data.insert(
            "failure_kind".to_string(),
            json!("pr_lifecycle_persistence"),
        );
    }
    instance
}

pub(super) async fn upsert_github_issue_pr_definition(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await
}

pub(super) async fn load_or_issue_instance(
    store: &WorkflowRuntimeStore,
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> anyhow::Result<(WorkflowInstance, bool)> {
    Ok(match store.get_instance(&workflow_id).await? {
        Some(instance) => (instance, false),
        None => (
            issue_instance(workflow_id, project_id, repo, issue_number, state),
            true,
        ),
    })
}

pub(super) async fn load_or_pr_runtime_target(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    repo: Option<&str>,
    issue_number: Option<u64>,
    pr_number: u64,
    task_id: &TaskId,
    pr_url: Option<&str>,
    state: &str,
) -> anyhow::Result<PrRuntimeTarget> {
    upsert_github_issue_pr_definition(store).await?;
    let project_id = project_root.to_string_lossy().into_owned();
    if let Some(issue_number) = issue_number {
        let workflow_id =
            harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number);
        let (instance, new_instance) = load_or_issue_instance(
            store,
            workflow_id,
            project_id,
            repo.map(ToOwned::to_owned),
            issue_number,
            state,
        )
        .await?;
        return Ok(PrRuntimeTarget {
            instance,
            new_instance,
            issue_number: Some(issue_number),
        });
    }

    if let Some(instance) = store
        .get_instance_by_pr(GITHUB_ISSUE_PR_DEFINITION_ID, &project_id, repo, pr_number)
        .await?
    {
        let issue_number = instance
            .data
            .get("issue_number")
            .and_then(serde_json::Value::as_u64);
        return Ok(PrRuntimeTarget {
            instance,
            new_instance: false,
            issue_number,
        });
    }

    Ok(PrRuntimeTarget {
        instance: pr_scoped_instance(
            pr_workflow_id(&project_id, repo, pr_number),
            project_id,
            repo.map(ToOwned::to_owned),
            task_id,
            pr_number,
            pr_url,
            state,
        ),
        new_instance: true,
        issue_number: None,
    })
}

pub(super) fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(crate::workflow_runtime_policy::merge_runtime_retry_policy(
        Path::new(&project_id),
        json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
        }),
    ))
}

pub(super) fn pr_scoped_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
    state: &str,
) -> WorkflowInstance {
    let data = pr_runtime_data(
        Path::new(&project_id),
        project_id.clone(),
        repo.as_deref(),
        None,
        task_id,
        pr_number,
        pr_url,
        None,
    );
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("pr", format!("pr:{pr_number}")),
    )
    .with_id(workflow_id)
    .with_data(data)
}

pub(super) fn pr_workflow_id(project_id: &str, repo: Option<&str>, pr_number: u64) -> String {
    format!(
        "{project_id}::repo:{}::pr:{pr_number}:feedback",
        repo.unwrap_or("<none>")
    )
}

pub(super) fn pr_runtime_data(
    project_root: &Path,
    project_id: String,
    repo: Option<&str>,
    issue_number: Option<u64>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
    feedback_summary: Option<&str>,
) -> serde_json::Value {
    let mut data = json!({
        "project_id": project_id,
        "repo": repo,
        "task_id": task_id.as_str(),
        "pr_number": pr_number,
        "pr_url": pr_url,
    });
    if let Some(issue_number) = issue_number {
        data["issue_number"] = json!(issue_number);
    }
    if let Some(feedback_summary) = feedback_summary {
        data["feedback_summary"] = json!(feedback_summary);
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(project_root, data)
}

pub(super) fn merge_last_decision(
    mut data: serde_json::Value,
    decision: &str,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
    }
    data
}

pub(super) fn merge_runtime_merge_data(
    mut data: serde_json::Value,
    decision: &str,
    task_id: Option<&str>,
    delete_branch: bool,
    require_review_threads_resolved: bool,
    require_clean_merge_state: bool,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert("merge_approved_at".to_string(), json!(chrono::Utc::now()));
        object.insert("merge_delete_branch".to_string(), json!(delete_branch));
        object.insert(
            "merge_require_review_threads_resolved".to_string(),
            json!(require_review_threads_resolved),
        );
        object.insert(
            "merge_require_clean_merge_state".to_string(),
            json!(require_clean_merge_state),
        );
        if let Some(task_id) = task_id {
            object.insert("merge_approved_task_id".to_string(), json!(task_id));
        }
    }
    data
}

pub(super) fn optional_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn optional_bool_field(data: &serde_json::Value, field: &str) -> Option<bool> {
    data.get(field).and_then(|value| value.as_bool())
}

pub(super) fn required_u64_field(data: &serde_json::Value, field: &str) -> anyhow::Result<u64> {
    data.get(field)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing {field}"))
}

pub(super) fn event_type(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "FeedbackFound",
        PrFeedbackOutcome::NoActionableFeedback => "NoFeedbackFound",
        PrFeedbackOutcome::ReadyToMerge => "PrReadyToMerge",
    }
}

pub(super) fn outcome_label(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "blocking_feedback",
        PrFeedbackOutcome::NoActionableFeedback => "no_actionable_feedback",
        PrFeedbackOutcome::ReadyToMerge => "ready_to_merge",
    }
}
