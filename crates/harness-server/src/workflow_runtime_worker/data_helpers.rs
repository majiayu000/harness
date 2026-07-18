use anyhow::Context;
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    RuntimeJob, WorkflowInstance, WorkflowRuntimeStore, PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn activity_name(job: &RuntimeJob) -> String {
    job.input
        .get("activity")
        .and_then(Value::as_str)
        .filter(|activity| !activity.trim().is_empty())
        .or_else(|| {
            job.input
                .get("command")
                .and_then(|command| command.get("activity"))
                .and_then(Value::as_str)
                .filter(|activity| !activity.trim().is_empty())
        })
        .or_else(|| {
            job.input
                .get("command_type")
                .and_then(Value::as_str)
                .filter(|command_type| !command_type.trim().is_empty())
        })
        .unwrap_or("workflow_activity")
        .to_string()
}

pub(super) fn is_builtin_lifecycle_activity(job: &RuntimeJob) -> bool {
    activity_name(job) == "start_child_workflow"
}

pub(super) fn required_string<'a>(value: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("start_child_workflow `{field}` is missing"))
}

pub(super) fn optional_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn string_vec(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(super) fn dependency_task_ids_from_command(
    command: &Value,
    repo: Option<&str>,
    issue_task_prefix: Option<&str>,
) -> Vec<TaskId> {
    command
        .get("depends_on")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| dependency_task_id(value, repo, issue_task_prefix))
                .collect()
        })
        .unwrap_or_default()
}

fn dependency_task_id(
    value: &Value,
    repo: Option<&str>,
    issue_task_prefix: Option<&str>,
) -> Option<TaskId> {
    if let Some(issue_number) = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    {
        return Some(TaskId::from_str(&issue_task_id_for_number(
            issue_number,
            repo,
            issue_task_prefix,
        )));
    }
    value
        .as_str()
        .filter(|raw| !raw.trim().is_empty())
        .map(TaskId::from_str)
}

pub(super) fn issue_task_id_for_number(
    issue_number: u64,
    repo: Option<&str>,
    issue_task_prefix: Option<&str>,
) -> String {
    match issue_task_prefix {
        Some(prefix) => format!("{prefix}:issue:{issue_number}"),
        None => format!(
            "github-issue:{}:issue:{issue_number}",
            repo.unwrap_or("<none>")
        ),
    }
}

pub(super) fn issue_task_id_from_command(
    command: &Value,
    job: &RuntimeJob,
    repo: Option<&str>,
    issue_number: u64,
) -> TaskId {
    let task_id = optional_string(command, "task_id")
        .or_else(|| {
            job.input
                .get("dedupe_key")
                .and_then(Value::as_str)
                .and_then(|dedupe_key| issue_task_id_from_dedupe_key(dedupe_key, issue_number))
        })
        .unwrap_or_else(|| issue_task_id_for_number(issue_number, repo, None));
    TaskId::from_str(&task_id)
}

pub(super) fn issue_task_prefix_from_task_id(
    task_id: &TaskId,
    issue_number: u64,
) -> Option<String> {
    let marker = format!(":issue:{issue_number}");
    let raw = task_id.as_str();
    let marker_start = raw.find(&marker)?;
    let prefix = &raw[..marker_start];
    (!prefix.is_empty()).then(|| prefix.to_string())
}

fn issue_task_id_from_dedupe_key(dedupe_key: &str, issue_number: u64) -> Option<String> {
    let marker = format!(":issue:{issue_number}");
    let marker_end = dedupe_key.find(&marker)? + marker.len();
    Some(dedupe_key[..marker_end].to_string()).filter(|task_id| !task_id.is_empty())
}

pub(super) fn force_execute_from_project_policy(project_id: &str, labels: &[String]) -> bool {
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(Path::new(project_id))
        .unwrap_or_default();
    labels
        .iter()
        .any(|label| label == &workflow_cfg.issue_workflow.force_execute_label)
}

pub(super) fn optional_data_u64(workflow: &WorkflowInstance, field: &str) -> Option<u64> {
    workflow.data.get(field).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    })
}

pub(super) fn optional_data_string(workflow: &WorkflowInstance, field: &str) -> Option<String> {
    workflow
        .data
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn parse_issue_subject_key(subject_key: &str) -> anyhow::Result<u64> {
    subject_key
        .strip_prefix("issue:")
        .unwrap_or(subject_key)
        .parse::<u64>()
        .with_context(|| format!("start_child_workflow subject_key `{subject_key}` is invalid"))
}

pub(super) fn parse_pr_subject_key(subject_key: &str) -> Option<u64> {
    subject_key
        .strip_prefix("pr:")
        .unwrap_or(subject_key)
        .parse::<u64>()
        .ok()
}

pub(super) fn merge_child_issue_data(
    mut data: Value,
    project_id: &str,
    repo: Option<&str>,
    issue_number: u64,
    runtime_job_id: &str,
    command_id: &str,
) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    if let Some(object) = data.as_object_mut() {
        object.insert("project_id".to_string(), json!(project_id));
        object.insert("repo".to_string(), json!(repo));
        object.insert("issue_number".to_string(), json!(issue_number));
        object.insert(
            "started_by_runtime_job_id".to_string(),
            json!(runtime_job_id),
        );
        object.insert("started_by_command_id".to_string(), json!(command_id));
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(Path::new(project_id), data)
}

pub(super) struct PrFeedbackChildData<'a> {
    pub project_id: &'a str,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub parent_workflow_id: &'a str,
    pub runtime_job_id: &'a str,
    pub command_id: &'a str,
}

pub(super) fn merge_pr_feedback_child_data(
    mut data: Value,
    input: PrFeedbackChildData<'_>,
) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    if let Some(object) = data.as_object_mut() {
        object.insert("project_id".to_string(), json!(input.project_id));
        object.insert("repo".to_string(), json!(input.repo));
        object.insert("issue_number".to_string(), json!(input.issue_number));
        object.insert("pr_number".to_string(), json!(input.pr_number));
        object.insert("pr_url".to_string(), json!(input.pr_url));
        object.insert(
            "parent_workflow_id".to_string(),
            json!(input.parent_workflow_id),
        );
        object.insert(
            "started_by_runtime_job_id".to_string(),
            json!(input.runtime_job_id),
        );
        object.insert("started_by_command_id".to_string(), json!(input.command_id));
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(Path::new(input.project_id), data)
}

pub(super) fn merge_json_object(target: &mut Value, update: Value) {
    let Some(target_object) = target.as_object_mut() else {
        return;
    };
    let Some(update_object) = update.as_object() else {
        return;
    };
    for (key, value) in update_object {
        target_object.insert(key.clone(), value.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PromptTaskRequest {
    NotPromptActivity,
    Ready(String),
    PayloadUnavailable { prompt_ref: String },
}

impl PromptTaskRequest {
    pub(super) fn prompt_text(&self) -> Option<&str> {
        match self {
            Self::Ready(prompt) => Some(prompt.as_str()),
            Self::NotPromptActivity | Self::PayloadUnavailable { .. } => None,
        }
    }
}

pub(super) async fn prompt_task_request_for_job(
    job: &RuntimeJob,
    store: Option<&WorkflowRuntimeStore>,
) -> anyhow::Result<PromptTaskRequest> {
    if activity_name(job) != PROMPT_TASK_IMPLEMENT_ACTIVITY {
        return Ok(PromptTaskRequest::NotPromptActivity);
    }
    let Some(prompt_ref) = job
        .input
        .get("command")
        .and_then(|command| command.get("prompt_ref"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        anyhow::bail!("runtime prompt task command is missing prompt_ref");
    };
    if let Some(store) = store {
        if let Some(prompt) =
            crate::workflow_runtime_submission::lookup_prompt_submission_prompt_durable(
                store, prompt_ref,
            )
            .await?
        {
            return Ok(PromptTaskRequest::Ready(prompt));
        }
    } else if let Some(prompt) =
        crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref)
    {
        return Ok(PromptTaskRequest::Ready(prompt));
    }
    Ok(PromptTaskRequest::PayloadUnavailable {
        prompt_ref: prompt_ref.to_string(),
    })
}

pub(super) fn prompt_payload_unavailable_result(
    job: &RuntimeJob,
    prompt_ref: &str,
) -> harness_workflow::runtime::ActivityResult {
    use harness_workflow::runtime::{
        ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus,
    };
    let activity = activity_name(job);
    let error = format!(
        "Runtime prompt payload `{prompt_ref}` is unavailable because prompt text is only held in the current Harness process."
    );
    ActivityResult {
        activity,
        status: ActivityStatus::Blocked,
        summary: "Runtime prompt task is blocked until the prompt is resubmitted.".to_string(),
        artifacts: vec![ActivityArtifact::new(
            "prompt_payload_unavailable",
            json!({
                "prompt_ref": prompt_ref,
                "recovery": "Resubmit the prompt task so Harness can rebuild the runtime prompt payload."
            }),
        )],
        signals: Vec::new(),
        validation: Vec::new(),
        error: Some(error),
        error_kind: Some(ActivityErrorKind::Configuration),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{
        ActivityErrorKind, ActivityStatus, RuntimeKind, PROMPT_TASK_IMPLEMENT_ACTIVITY,
    };

    #[test]
    fn activity_name_uses_top_level_runtime_activity_key() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": "start_child_workflow",
                "command_type": "start_child_workflow",
                "command": {
                    "definition_id": "github_issue_pr",
                    "subject_key": "issue:123"
                }
            }),
        );

        assert_eq!(activity_name(&job), "start_child_workflow");
    }

    #[test]
    fn activity_name_falls_back_to_command_type() {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "command_type": "start_child_workflow",
                "command": {
                    "definition_id": "github_issue_pr",
                    "subject_key": "issue:123"
                }
            }),
        );

        assert_eq!(activity_name(&job), "start_child_workflow");
    }

    #[tokio::test]
    async fn prompt_task_request_blocks_when_cached_payload_is_unavailable() -> anyhow::Result<()> {
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "command": {
                    "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                    "prompt_ref": "prompt-submission:cache-miss-test"
                }
            }),
        );

        let request = prompt_task_request_for_job(&job, None).await?;
        assert_eq!(
            request,
            PromptTaskRequest::PayloadUnavailable {
                prompt_ref: "prompt-submission:cache-miss-test".to_string()
            }
        );

        let result = prompt_payload_unavailable_result(&job, "prompt-submission:cache-miss-test");
        assert_eq!(result.status, ActivityStatus::Blocked);
        assert_eq!(result.activity, PROMPT_TASK_IMPLEMENT_ACTIVITY);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("only held in the current Harness process"));
        assert_eq!(
            result.artifacts[0].artifact_type,
            "prompt_payload_unavailable"
        );
        Ok(())
    }

    #[tokio::test]
    async fn prompt_task_request_recovers_persisted_payload_after_cache_miss() -> anyhow::Result<()>
    {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let database_url = crate::test_helpers::test_database_url()?;
        let store = WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir(&project_root)?;
        let task_id = TaskId::from_str("restart-safe-prompt-task");
        let submission = crate::workflow_runtime_submission::record_prompt_submission(
            &store,
            crate::workflow_runtime_submission::PromptSubmissionRuntimeContext {
                project_root: &project_root,
                task_id: &task_id,
                prompt: "restart safe prompt",
                depends_on: &[],
                serialization_depends_on: &[],
                dependencies_blocked: false,
                source: Some("test"),
                external_id: Some("manual:restart-safe"),
                continuation: None,
            },
        )
        .await?;
        let workflow = store
            .get_instance(&submission.workflow_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("prompt workflow should be persisted"))?;
        let prompt_ref = workflow.data["prompt_ref"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("prompt ref should be persisted"))?;
        crate::workflow_runtime_submission::clear_prompt_submission_prompt_cache_for_test(
            prompt_ref,
        );
        let job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "command": {
                    "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                    "prompt_ref": prompt_ref
                }
            }),
        );

        let request = prompt_task_request_for_job(&job, Some(&store)).await?;

        assert_eq!(
            request,
            PromptTaskRequest::Ready("restart safe prompt".to_string())
        );
        assert_eq!(
            crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref)
                .as_deref(),
            Some("restart safe prompt")
        );
        Ok(())
    }

    #[test]
    fn dependency_task_ids_from_command_maps_issue_numbers_to_repo_handles() {
        let command = json!({
            "depends_on": [42, "43", "explicit-task-id"]
        });

        let dependencies = dependency_task_ids_from_command(&command, Some("owner/repo"), None);

        assert_eq!(
            dependencies
                .iter()
                .map(|task_id| task_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "github-issue:owner/repo:issue:42",
                "github-issue:owner/repo:issue:43",
                "explicit-task-id"
            ]
        );
    }

    #[test]
    fn dependency_task_ids_from_command_preserves_prompt_child_prefix() {
        let command = json!({
            "depends_on": [42, "43", "explicit-task-id"]
        });

        let dependencies = dependency_task_ids_from_command(
            &command,
            Some("owner/repo"),
            Some("prompt-task:owner/repo"),
        );

        assert_eq!(
            dependencies
                .iter()
                .map(|task_id| task_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "prompt-task:owner/repo:issue:42",
                "prompt-task:owner/repo:issue:43",
                "explicit-task-id"
            ]
        );
    }

    #[test]
    fn issue_task_id_from_command_prefers_explicit_task_id() {
        let command = json!({
            "task_id": "explicit-task",
        });
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "dedupe_key": "prompt-task:owner/repo:issue:42:start",
            }),
        );

        let task_id = issue_task_id_from_command(&command, &job, Some("owner/repo"), 42);

        assert_eq!(task_id.as_str(), "explicit-task");
    }

    #[test]
    fn issue_task_id_from_command_preserves_dedupe_issue_namespace() {
        let command = json!({});
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({
                "dedupe_key": "prompt-task:owner/repo:issue:42:start:retry",
            }),
        );

        let task_id = issue_task_id_from_command(&command, &job, Some("owner/repo"), 42);

        assert_eq!(task_id.as_str(), "prompt-task:owner/repo:issue:42");
        assert_eq!(
            issue_task_prefix_from_task_id(&task_id, 42).as_deref(),
            Some("prompt-task:owner/repo")
        );
    }
}
