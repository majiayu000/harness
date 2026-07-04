use super::*;
use harness_core::db::resolve_database_url;

async fn open_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

fn expect_cancelled(outcome: RuntimeSubmissionCancelOutcome, context: &str) -> WorkflowInstance {
    match outcome {
        RuntimeSubmissionCancelOutcome::Cancelled(workflow) => workflow,
        other => panic!("{context}: expected cancelled workflow, got {other:?}"),
    }
}

#[tokio::test]
async fn issue_submission_records_pending_runtime_implementation_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("task-1");
    let labels = vec!["bug".to_string(), "force-execute".to_string()];

    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 42,
            task_id: &task_id,
            labels: &labels,
            force_execute: true,
            additional_prompt: Some("include the regression test first"),
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:42"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    assert!(result.accepted);
    assert_eq!(result.command_ids.len(), 1);
    assert!(result.rejection_reason.is_none());

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should be persisted");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], "task-1");
    assert_eq!(instance.data["task_ids"], serde_json::json!(["task-1"]));
    assert_eq!(
        instance.data["additional_prompt"],
        "include the regression test first"
    );
    assert_eq!(instance.data["source"], "github");
    assert_eq!(instance.data["external_id"], "issue:42");
    assert_eq!(instance.data["last_decision"], "submit_issue");
    assert_eq!(
        instance.data["execution_path"],
        EXECUTION_PATH_WORKFLOW_RUNTIME
    );

    let events = store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "IssueSubmitted"));

    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
    assert_eq!(
        commands[0].command.command["additional_prompt"],
        "include the regression test first"
    );
    assert_eq!(store.pending_commands(10).await?.len(), 1);
    assert!(store
        .runtime_jobs_for_command(&commands[0].id)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn prompt_submission_records_pending_runtime_implementation_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("prompt-task-1");

    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "fix the prompt-only issue",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some("manual:prompt:1"),
        },
    )
    .await?;

    assert!(result.accepted);
    assert_eq!(result.command_ids.len(), 1);
    assert!(result.rejection_reason.is_none());

    let workflow_id = prompt_workflow_id(
        &project_root.to_string_lossy(),
        Some("manual:prompt:1"),
        &task_id,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow should be persisted");
    assert_eq!(instance.definition_id, PROMPT_TASK_DEFINITION_ID);
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], "prompt-task-1");
    assert_eq!(
        instance.data["task_ids"],
        serde_json::json!(["prompt-task-1"])
    );
    assert!(instance.data.get("prompt").is_none());
    assert_eq!(instance.data["prompt_summary"], PROMPT_TASK_DESCRIPTION);
    assert_eq!(
        instance.data["prompt_chars"],
        "fix the prompt-only issue".chars().count()
    );
    let prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be persisted");
    assert_eq!(
        lookup_prompt_submission_prompt(prompt_ref).as_deref(),
        Some("fix the prompt-only issue")
    );
    assert_eq!(instance.data["source"], "dashboard");
    assert_eq!(instance.data["external_id"], "manual:prompt:1");
    assert_eq!(instance.data["last_decision"], "submit_prompt");
    assert_eq!(
        instance.data["execution_path"],
        EXECUTION_PATH_WORKFLOW_RUNTIME
    );

    let events = store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "PromptSubmitted"));

    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    assert!(commands[0].command.command.get("prompt").is_none());
    assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
    assert_eq!(store.pending_commands(10).await?.len(), 1);
    assert!(store
        .runtime_jobs_for_command(&commands[0].id)
        .await?
        .is_empty());
    Ok(())
}

#[test]
fn terminal_prompt_submission_removes_cached_prompt() {
    let prompt_ref = "prompt-memory:test-cleanup";
    cache_prompt_submission_prompt(prompt_ref, "sensitive prompt body");
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "done",
        WorkflowSubject::new("prompt", "manual:prompt:cleanup"),
    )
    .with_data(json!({
        "task_id": "runtime-prompt-cleanup",
        "prompt_ref": prompt_ref,
    }));

    remove_terminal_prompt_submission_prompt(&instance);

    assert_eq!(lookup_prompt_submission_prompt(prompt_ref), None);
}

#[tokio::test]
async fn prompt_resubmission_removes_previous_cached_prompt() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let task_id = TaskId::from_str("runtime-prompt-cache-retry");
    let external_id = "manual:prompt:cache-retry";
    let old_prompt_ref = "prompt-memory:test-resubmit-old";
    cache_prompt_submission_prompt(old_prompt_ref, "old sensitive prompt body");
    let workflow_id = prompt_workflow_id(&project_id, Some(external_id), &task_id);
    store
        .upsert_instance(
            &WorkflowInstance::new(
                PROMPT_TASK_DEFINITION_ID,
                1,
                "blocked",
                WorkflowSubject::new("prompt", external_id),
            )
            .with_id(workflow_id)
            .with_data(json!({
                "project_id": project_id,
                "task_id": "old-prompt-task",
                "prompt_ref": old_prompt_ref,
                "source": "dashboard",
                "external_id": external_id,
            })),
        )
        .await?;

    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "new prompt body",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some("dashboard"),
            external_id: Some(external_id),
        },
    )
    .await?;

    assert!(result.accepted);
    assert_eq!(lookup_prompt_submission_prompt(old_prompt_ref), None);
    let instance = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    let new_prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("new prompt ref should be persisted");
    assert_ne!(new_prompt_ref, old_prompt_ref);
    assert_eq!(
        lookup_prompt_submission_prompt(new_prompt_ref).as_deref(),
        Some("new prompt body")
    );
    Ok(())
}

#[tokio::test]
async fn issue_submission_preserves_prior_task_handles_for_lookup() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let labels = vec!["bug".to_string()];
    let first_task_id = TaskId::from_str("runtime-handle-first");
    let second_task_id = TaskId::from_str("runtime-handle-second");

    record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 44,
            task_id: &first_task_id,
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
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 44,
            task_id: &second_task_id,
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

    assert!(result.accepted);
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(
        workflow.data["task_ids"],
        serde_json::json!(["runtime-handle-first", "runtime-handle-second"])
    );
    assert_eq!(
        runtime_issue_task_handle(&workflow)
            .expect("runtime workflow should expose a stable handle")
            .as_str(),
        "runtime-handle-first"
    );
    assert_eq!(
        runtime_issue_by_submission_id(&store, &first_task_id)
            .await?
            .expect("first handle should resolve")
            .id,
        workflow.id
    );
    assert!(
        runtime_issue_by_submission_id(&store, &second_task_id)
            .await?
            .is_none(),
        "retry task id should not remain a public lookup alias after submission_id is explicit"
    );
    Ok(())
}

#[tokio::test]
async fn cancel_issue_submission_cancels_dispatched_runtime_jobs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-handle-cancel");
    let labels = vec!["bug".to_string()];
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 42,
            task_id: &task_id,
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
    let command_id = result
        .command_ids
        .first()
        .expect("issue submission should enqueue an implementation command")
        .clone();
    store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "default",
            json!({ "task_id": task_id.as_str() }),
            None,
        )
        .await?;
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("command should exist")
            .status,
        "dispatched"
    );

    let cancelled = expect_cancelled(
        cancel_issue_submission_by_task_id(&store, &task_id).await?,
        "runtime issue submission should resolve by task id",
    );
    assert_eq!(cancelled.state, "cancelled");

    let commands = store.commands_for(&result.workflow_id).await?;
    let original_command = commands
        .iter()
        .find(|command| command.id == command_id)
        .expect("original implementation command should remain visible");
    assert_eq!(original_command.status, "cancelled");
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].status,
        harness_workflow::runtime::RuntimeJobStatus::Cancelled
    );
    assert!(jobs[0].lease.is_none());
    Ok(())
}

#[tokio::test]
async fn cancel_issue_submission_cancels_dispatching_runtime_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-handle-cancel-dispatching");
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 44,
            task_id: &task_id,
            labels: &[],
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
    let command_id = result
        .command_ids
        .first()
        .expect("issue submission should enqueue an implementation command")
        .clone();
    let claimed = store
        .claim_pending_commands(
            "dispatcher-a",
            chrono::Utc::now() + chrono::Duration::seconds(60),
            10,
        )
        .await?;
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, command_id);
    assert_eq!(claimed[0].status, "dispatching");

    let cancelled = expect_cancelled(
        cancel_issue_submission_by_task_id(&store, &task_id).await?,
        "runtime issue submission should resolve by task id",
    );
    assert_eq!(cancelled.state, "cancelled");

    let command = store
        .get_command(&command_id)
        .await?
        .expect("command should remain visible");
    assert_eq!(command.status, "cancelled");
    assert!(command.dispatch_owner.is_none());
    assert!(command.dispatch_lease_expires_at.is_none());
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());

    let outcomes = harness_workflow::runtime::RuntimeCommandDispatcher::new(
        &store,
        harness_workflow::runtime::RuntimeProfile::new(
            "codex-default",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
    )
    .dispatch_pending()
    .await?;
    assert!(outcomes.is_empty());
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn cancel_issue_submission_by_workflow_id_uses_stable_runtime_handle() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-handle-cancel-by-workflow");
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 43,
            task_id: &task_id,
            labels: &[],
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

    let cancelled = expect_cancelled(
        cancel_submission_by_workflow_id(&store, &result.workflow_id).await?,
        "runtime issue submission should resolve by workflow id",
    );
    assert_eq!(cancelled.state, "cancelled");
    assert_eq!(cancelled.data["cancelled"], true);
    assert_eq!(cancelled.data["last_decision"], "cancel_issue_submission");
    let events = store.events_for(&result.workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "IssueSubmissionCancelled"));
    Ok(())
}

#[tokio::test]
async fn cancel_prompt_submission_cancels_dispatched_runtime_jobs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::from_str("runtime-prompt-handle-cancel");
    let result = record_prompt_submission(
        &store,
        PromptSubmissionRuntimeContext {
            project_root: &project_root,
            task_id: &task_id,
            prompt: "cancel this prompt runtime task",
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
        },
    )
    .await?;
    let command_id = result
        .command_ids
        .first()
        .expect("prompt submission should enqueue an implementation command")
        .clone();
    store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "default",
            json!({ "task_id": task_id.as_str() }),
            None,
        )
        .await?;
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("command should exist")
            .status,
        "dispatched"
    );

    let cancelled = expect_cancelled(
        cancel_issue_submission_by_task_id(&store, &task_id).await?,
        "runtime prompt submission should resolve by task id",
    );
    assert_eq!(cancelled.state, "cancelled");

    let commands = store.commands_for(&result.workflow_id).await?;
    let original_command = commands
        .iter()
        .find(|command| command.id == command_id)
        .expect("original implementation command should remain visible");
    assert_eq!(original_command.status, "cancelled");
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].status,
        harness_workflow::runtime::RuntimeJobStatus::Cancelled
    );
    assert!(jobs[0].lease.is_none());
    Ok(())
}

#[tokio::test]
async fn rejected_issue_submission_keeps_existing_runtime_data() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 42);
    let mut existing = issue_instance(
        workflow_id.clone(),
        project_id,
        Some("owner/repo".to_string()),
        42,
    );
    existing.state = "pr_open".to_string();
    existing.data = serde_json::json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "older-task",
        "pr_url": "https://github.com/owner/repo/pull/99",
        "last_decision": "bind_pr"
    });
    let original_data = existing.data.clone();
    store.upsert_instance(&existing).await?;

    let task_id = TaskId::from_str("new-task");
    let labels = vec!["bug".to_string()];
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 42,
            task_id: &task_id,
            labels: &labels,
            force_execute: false,
            additional_prompt: Some("do not clobber existing metadata"),
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;

    assert!(!result.accepted);
    assert!(result.command_ids.is_empty());
    assert!(result
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("TransitionNotAllowed")));

    let persisted = store
        .get_instance(&workflow_id)
        .await?
        .expect("existing workflow should remain persisted");
    assert_eq!(persisted.state, "pr_open");
    assert_eq!(persisted.data, original_data);
    assert!(store.commands_for(&workflow_id).await?.is_empty());

    let decisions = store.decisions_for(&workflow_id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].accepted);
    assert!(decisions[0]
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("TransitionNotAllowed")));

    let events = store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "IssueSubmitted"));
    Ok(())
}

#[tokio::test]
async fn issue_submission_waits_for_dependencies_then_releases_runtime_command(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = open_runtime_store(dir.path()).await?;
    let task_store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let dep_id = TaskId::from_str("dep-1");
    let mut dep = crate::task_runner::TaskState::new(dep_id.clone());
    dep.status = TaskStatus::Pending;
    task_store.insert(&dep).await;

    let task_id = TaskId::from_str("runtime-handle-1");
    let labels = vec!["bug".to_string()];
    let result = record_issue_submission(
        &store,
        IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 77,
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

    assert!(result.accepted);
    assert!(result.command_ids.is_empty());
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should be persisted");
    assert_eq!(workflow.state, "awaiting_dependencies");
    assert_eq!(workflow.data["depends_on"], serde_json::json!(["dep-1"]));
    assert!(store.commands_for(&result.workflow_id).await?.is_empty());

    let waiting = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(waiting.waiting, 1);
    assert_eq!(waiting.released, 0);

    dep.status = TaskStatus::Done;
    task_store.insert(&dep).await;
    let released = release_ready_issue_dependencies(&store, &task_store, 10).await?;
    assert_eq!(released.released, 1);
    let workflow = store
        .get_instance(&result.workflow_id)
        .await?
        .expect("workflow should remain persisted");
    assert_eq!(workflow.state, "planning");
    assert_eq!(workflow.data["dependencies_blocked"], false);
    let commands = store.commands_for(&result.workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));
    Ok(())
}
