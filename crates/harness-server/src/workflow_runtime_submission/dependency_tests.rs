use super::*;
use harness_core::db::resolve_database_url;
use harness_workflow::runtime::{
    ActivityResult, ActivitySignal, ActivityStatus, RUNTIME_JOB_COMPLETED_EVENT,
};

async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

#[tokio::test]
async fn prompt_submission_waits_for_dependencies_then_releases_runtime_command(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let dep_id = TaskId::from_str("prompt-dep-1");
    let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Pending;
    task_store.insert(&dep).await;

    let task_id = TaskId::from_str("runtime-prompt-handle-1");
    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "wait until dependency is done",
            depends_on: std::slice::from_ref(&dep_id),
            serialization_depends_on: &[],
            dependencies_blocked: true,
            source: None,
            external_id: None,
        },
    )
    .await?;

    assert!(result.accepted);
    assert!(result.command_ids.is_empty());
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should be persisted");
    assert_eq!(workflow.state, "awaiting_dependencies");
    assert_eq!(
        workflow.data["depends_on"],
        serde_json::json!(["prompt-dep-1"])
    );
    assert!(store.commands_for(&result.workflow_id).await?.is_empty());

    let waiting = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
    assert_eq!(waiting.waiting, 1);
    assert_eq!(waiting.released, 0);

    dep.status = TaskStatus::Done;
    task_store.insert(&dep).await;
    let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(workflow.state, "implementing");
    assert_eq!(workflow.data["dependencies_blocked"], false);
    let commands = store.commands_for(&result.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    Ok(())
}

#[tokio::test]
async fn prompt_submission_releases_after_failed_serialization_dependency() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let dep_id = TaskId::from_str("prompt-serialization-dep");
    let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Failed;
    task_store.insert(&dep).await;

    let task_id = TaskId::from_str("runtime-prompt-serialized");
    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "run after serialized predecessor reaches terminal state",
            depends_on: &[],
            serialization_depends_on: std::slice::from_ref(&dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
        },
    )
    .await?;

    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should be persisted");
    assert_eq!(workflow.state, "awaiting_dependencies");
    assert_eq!(
        workflow.data["depends_on"],
        serde_json::json!(["prompt-serialization-dep"])
    );
    assert_eq!(
        workflow.data["serialization_depends_on"],
        serde_json::json!(["prompt-serialization-dep"])
    );

    let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    assert_eq!(released.failed, 0);
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(workflow.state, "implementing");
    assert_eq!(workflow.data["dependencies_blocked"], false);
    assert_eq!(store.commands_for(&result.workflow_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn prompt_submission_recovers_release_when_in_memory_prompt_is_missing() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let dep_id = TaskId::from_str("prompt-missing-dep");
    let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Done;
    task_store.insert(&dep).await;

    let task_id = TaskId::from_str("runtime-prompt-missing-cache");
    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "lost after server restart",
            depends_on: std::slice::from_ref(&dep_id),
            serialization_depends_on: &[],
            dependencies_blocked: true,
            source: None,
            external_id: None,
        },
    )
    .await?;

    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should be persisted");
    let prompt_ref = workflow.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be persisted")
        .to_string();
    remove_prompt_submission_prompt(Some(&prompt_ref));

    let released = release_ready_prompt_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    assert_eq!(released.failed, 0);
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(workflow.state, "implementing");
    let commands = store.commands_for(&result.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.command_type,
        WorkflowCommandType::EnqueueActivity
    );
    Ok(())
}

#[tokio::test]
async fn issue_submission_releases_dependency_on_completed_runtime_handle() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let dep_id = TaskId::from_str("runtime-dep-handle");
    let dep_result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 78,
            task_id: &dep_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let mut dep_workflow = store
        .get_instance(&dep_result.workflow_id)
        .await?
        .expect("dependency workflow should exist");
    dep_workflow.state = "done".to_string();
    store.upsert_instance(&dep_workflow).await?;

    let task_id = TaskId::from_str("runtime-dependent-handle");
    let blocked = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 79,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    let workflow = store
        .get_instance(&blocked.workflow_id)
        .await?
        .expect("dependent workflow should remain persisted");
    assert_eq!(workflow.state, "planning");
    let commands = store.commands_for(&blocked.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}

#[tokio::test]
async fn issue_submission_releases_dependency_by_github_issue_handle_when_canonical_workflow_completed(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let direct_dep_id = TaskId::from_str("direct-runtime-dep-handle");
    let dep_result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 78,
            task_id: &direct_dep_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let mut dep_workflow = store
        .get_instance(&dep_result.workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("dependency workflow should exist"))?;
    dep_workflow.state = "done".to_string();
    store.upsert_instance(&dep_workflow).await?;

    let github_issue_dep_id = TaskId::from_str("github-issue:owner/repo:issue:78");
    assert!(
        store
            .get_instance_by_task_id(github_issue_dep_id.as_str())
            .await?
            .is_none(),
        "canonical GitHub issue dependency handle should not exist as a runtime task id"
    );

    let task_id = TaskId::from_str("runtime-dependent-on-github-issue-handle");
    let blocked = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 79,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&github_issue_dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    assert_eq!(released.waiting, 0);
    assert_eq!(released.failed, 0);
    let workflow = store
        .get_instance(&blocked.workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("dependent workflow should remain persisted"))?;
    assert_eq!(workflow.state, "planning");
    let commands = store.commands_for(&blocked.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}

#[tokio::test]
async fn issue_submission_keeps_blocked_runtime_dependency_without_closed_evidence_waiting(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let dep_id = TaskId::from_str("runtime-blocked-no-evidence");
    let dep_result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 82,
            task_id: &dep_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let mut dep_workflow = store
        .get_instance(&dep_result.workflow_id)
        .await?
        .expect("dependency workflow should exist");
    dep_workflow.state = "blocked".to_string();
    store.upsert_instance(&dep_workflow).await?;

    let task_id = TaskId::from_str("runtime-dependent-on-blocked-no-evidence");
    record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 83,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let waiting = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(waiting.waiting, 1);
    assert_eq!(waiting.released, 0);
    assert_eq!(waiting.failed, 0);
    Ok(())
}

#[tokio::test]
async fn issue_submission_releases_blocked_runtime_dependency_with_persisted_closed_evidence(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let dep_id = TaskId::from_str("runtime-blocked-persisted-closed-evidence");
    let dep_result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 86,
            task_id: &dep_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let mut dep_workflow = store
        .get_instance(&dep_result.workflow_id)
        .await?
        .expect("dependency workflow should exist");
    dep_workflow.state = "blocked".to_string();
    dep_workflow.data["closed_issue_evidence"] = serde_json::json!({
        "source": "IssueClosed",
        "issue_number": 86,
        "state": "closed",
        "issue_url": "https://github.com/owner/repo/issues/86",
        "closed": true,
    });
    store.upsert_instance(&dep_workflow).await?;

    let task_id = TaskId::from_str("runtime-dependent-on-persisted-closed-evidence");
    let blocked = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 87,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    assert_eq!(released.waiting, 0);
    assert_eq!(released.failed, 0);
    let workflow = store
        .get_instance(&blocked.workflow_id)
        .await?
        .expect("dependent workflow should remain persisted");
    assert_eq!(workflow.state, "planning");
    let commands = store.commands_for(&blocked.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}

#[tokio::test]
async fn issue_submission_releases_blocked_runtime_dependency_with_closed_issue_evidence(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let dep_id = TaskId::from_str("runtime-blocked-closed-evidence");
    let dep_result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 84,
            task_id: &dep_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let mut dep_workflow = store
        .get_instance(&dep_result.workflow_id)
        .await?
        .expect("dependency workflow should exist");
    dep_workflow.state = "blocked".to_string();
    store.upsert_instance(&dep_workflow).await?;
    let activity_result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Issue was already resolved upstream.".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "IssueAlreadyResolved",
            serde_json::json!({
                "issue_number": 84,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/84",
            }),
        )],
        validation: Vec::new(),
        error: Some("No PR is needed because the upstream issue is closed.".to_string()),
        error_kind: None,
    };
    store
        .append_event(
            &dep_result.workflow_id,
            RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-1",
            serde_json::json!({
                "command_id": "command-1",
                "runtime_job_id": "job-1",
                "activity_result": activity_result,
            }),
        )
        .await?;

    let task_id = TaskId::from_str("runtime-dependent-on-blocked-closed-evidence");
    let blocked = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 85,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    assert_eq!(released.waiting, 0);
    assert_eq!(released.failed, 0);
    let workflow = store
        .get_instance(&blocked.workflow_id)
        .await?
        .expect("dependent workflow should remain persisted");
    assert_eq!(workflow.state, "planning");
    let commands = store.commands_for(&blocked.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}

#[tokio::test]
async fn dependency_release_rotates_waiting_rows_to_prevent_starvation() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];

    let old_dep_id = TaskId::from_str("old-blocked-dep");
    let old_dep = crate::task_runner::TaskState::new(old_dep_id.clone());
    task_store.insert(&old_dep).await;
    let old_task_id = TaskId::from_str("old-waiting-handle");
    record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 80,
            task_id: &old_task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&old_dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let ready_dep_id = TaskId::from_str("ready-dep");
    let mut ready_dep = crate::task_runner::TaskState::new(ready_dep_id.clone());
    ready_dep.status = TaskStatus::Done;
    task_store.insert(&ready_dep).await;
    let ready_task_id = TaskId::from_str("ready-waiting-handle");
    let ready = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 81,
            task_id: &ready_task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: std::slice::from_ref(&ready_dep_id),
            dependencies_blocked: true,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    let first = release_ready_issue_dependencies(&store, &task_store, 1).await?;
    assert_eq!(first.waiting, 1);
    assert_eq!(first.released, 0);
    let second = release_ready_issue_dependencies(&store, &task_store, 1).await?;
    assert_eq!(second.released, 1);
    let workflow = store
        .get_instance(&ready.workflow_id)
        .await?
        .expect("ready workflow should remain persisted");
    assert_eq!(workflow.state, "planning");
    let commands = store.commands_for(&ready.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}
