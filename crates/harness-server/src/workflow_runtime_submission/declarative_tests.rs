use super::*;
use chrono::{Duration, Utc};
use harness_core::{
    config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    },
    db::resolve_database_url,
};
use harness_workflow::runtime::{
    store::RuntimeJobEnqueueOutcome, ActivityResult, ActivitySignal, RuntimeKind,
    WorkflowCommandStatus, WorkflowCommandType,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Once,
};

const TEST_DEFINITION_ID: &str = "submission_test_declarative";
static REGISTER_TEST_DEFINITION: Once = Once::new();

async fn open_declarative_runtime_store(dir: &Path) -> anyhow::Result<WorkflowRuntimeStore> {
    let database_url = resolve_database_url(None)?;
    WorkflowRuntimeStore::open_with_database_url(dir, Some(&database_url)).await
}

fn register_test_definition() {
    REGISTER_TEST_DEFINITION.call_once(|| {
        let policy = WorkflowDefinitionPolicy {
            id: TEST_DEFINITION_ID.to_string(),
            initial: "working".to_string(),
            states: BTreeMap::from([
                (
                    "working".to_string(),
                    DeclaredState {
                        activity: Some("perform_work".to_string()),
                        on_success: Some("done".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_blocked: Some("blocked".to_string()),
                        on_signal: BTreeMap::from([(
                            "cancel".to_string(),
                            "cancelled".to_string(),
                        )]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["working".to_string()],
        };
        let definition = harness_workflow::runtime::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                "perform_work".to_string(),
                WorkflowActivityPolicy::default(),
            )]),
        )
        .expect("test declarative definition should be valid");
        harness_workflow::runtime::register_declarative_workflow_definitions([definition])
            .expect("test declarative definition should register");
    });
}

fn create_test_project(root: &Path) -> anyhow::Result<PathBuf> {
    let project_root = root.join("project");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        r#"---
definition:
  id: submission_test_declarative
  initial: working
  states:
    working:
      activity: perform_work
      on_success: done
      on_failure: failed
      on_blocked: blocked
      on_signal: { cancel: cancelled }
    blocked: { progress: operator_gate }
  terminal: { done: succeeded, failed: failed, cancelled: cancelled }
  recovery_targets: [working]
runtime_retry_policy:
  max_failed_activity_retries: 2
activities:
  perform_work: {}
---
Submission fixture.
"#,
    )?;
    Ok(project_root)
}

#[test]
fn declarative_submission_decision_validates_against_the_registered_submission_rule() {
    register_test_definition();
    let definition =
        harness_workflow::runtime::current_declarative_workflow_definition(TEST_DEFINITION_ID)
            .expect("definition should be registered");
    let instance = WorkflowInstance::new(
        TEST_DEFINITION_ID,
        definition.definition_version(),
        definition.policy().initial.clone(),
        WorkflowSubject::new("declarative", "task:validation"),
    )
    .with_data(json!({ "definition_hash": definition.definition_hash() }));
    let decision =
        harness_workflow::runtime::build_declarative_submission_decision(&definition, &instance)
            .expect("submission decision should build");
    let validator = harness_workflow::runtime::decision_validator_for_instance(&instance)
        .expect("pinned definition should be valid")
        .expect("pinned definition should resolve a validator");

    validator
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow-policy", chrono::Utc::now()),
        )
        .expect("registered declarative submission transition should validate");
}

#[tokio::test]
async fn declarative_submission_pins_immutable_definition_metadata() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_test_definition();
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let project_root = create_test_project(dir.path())?;
    let task_id = TaskId::from_str("declarative-pin");

    let record = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            definition_id: TEST_DEFINITION_ID,
            task_id: &task_id,
            prompt: "perform the declared work",
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some("test"),
            external_id: Some("pin-1"),
        },
    )
    .await?;
    let definition =
        harness_workflow::runtime::current_declarative_workflow_definition(TEST_DEFINITION_ID)
            .expect("definition should remain registered");
    let instance = store
        .get_instance(&record.workflow_id)
        .await?
        .expect("submission should persist an instance");

    assert_eq!(instance.definition_version, definition.definition_version());
    assert_eq!(
        instance.data["definition_hash"],
        definition.definition_hash()
    );
    assert_eq!(instance.state, definition.policy().initial);
    assert_eq!(instance.data["submission_id"], task_id.as_str());
    assert_eq!(
        instance.data["runtime_retry_policy"]["max_failed_activity_retries"],
        2
    );
    assert!(instance.data.get("prompt").is_none());
    let stored = store
        .get_definition(TEST_DEFINITION_ID, definition.definition_version())
        .await?
        .expect("definition metadata should be persisted");
    assert_eq!(stored.definition_hash, definition.definition_hash());
    assert_eq!(stored.metadata["kind"], "declarative_workflow");
    assert_eq!(stored.metadata["policy"]["id"], TEST_DEFINITION_ID);
    Ok(())
}

#[tokio::test]
async fn declarative_submission_enqueues_initial_activity_atomically() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_test_definition();
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let project_root = create_test_project(dir.path())?;
    let task_id = TaskId::from_str("declarative-command");

    let record = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            definition_id: TEST_DEFINITION_ID,
            task_id: &task_id,
            prompt: "perform the declared work",
            depends_on: &[],
            serialization_depends_on: &[],
            source: None,
            external_id: None,
        },
    )
    .await?;
    let commands = store.commands_for(&record.workflow_id).await?;

    assert!(record.accepted);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("perform_work"));
    assert_eq!(
        commands[0].command.dedupe_key,
        format!("{}:working:submit", record.workflow_id)
    );
    assert_eq!(commands[0].status, WorkflowCommandStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn declarative_submission_mapped_signal_reaches_terminal_state() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_test_definition();
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let project_root = create_test_project(dir.path())?;
    let task_id = TaskId::from_str("declarative-e2e");

    let record = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            definition_id: TEST_DEFINITION_ID,
            task_id: &task_id,
            prompt: "cancel through the declared signal mapping",
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some("stub-runtime-test"),
            external_id: Some("e2e-1"),
        },
    )
    .await?;
    let initial_commands = store.commands_for(&record.workflow_id).await?;
    let Some(initial_command) = initial_commands.first() else {
        anyhow::bail!("declarative submission did not enqueue its initial activity");
    };
    let runtime_job = match store
        .enqueue_runtime_job_for_pending_command(
            &initial_command.id,
            RuntimeKind::CodexJsonrpc,
            "stub-runtime",
            json!({"activity": "perform_work"}),
            None,
        )
        .await?
    {
        RuntimeJobEnqueueOutcome::Enqueued(job) => job,
        outcome => anyhow::bail!("initial activity was not dispatchable: {outcome:?}"),
    };
    let Some(claimed) = store
        .claim_next_runtime_job("stub-runtime", Utc::now() + Duration::minutes(5))
        .await?
    else {
        anyhow::bail!("stub runtime could not claim the initial activity");
    };
    assert_eq!(claimed.id, runtime_job.id);
    let Some(lease) = claimed.lease.as_ref() else {
        anyhow::bail!("claimed stub runtime job did not carry a lease");
    };
    let result = ActivityResult::succeeded("perform_work", "cancel requested")
        .with_signal(ActivitySignal::new("cancel", json!({"reason": "fixture"})));

    let Some(completion) = store
        .commit_runtime_activity_completion_if_owned(
            &claimed.id,
            "stub-runtime",
            lease.expires_at,
            &result,
        )
        .await?
    else {
        anyhow::bail!("stub runtime completion lost ownership");
    };
    let Some(decision) = completion.decision else {
        anyhow::bail!("mapped signal completion did not produce a workflow decision");
    };
    assert!(decision.accepted);
    assert_eq!(decision.decision.decision, "apply_declarative_transition");
    assert_eq!(decision.decision.next_state, "cancelled");
    assert_eq!(
        decision.decision.reason,
        "declarative activity 'perform_work' completed with Succeeded; on_signal 'cancel' selected state 'cancelled'"
    );
    assert_eq!(
        decision.decision.commands[0].command_type,
        WorkflowCommandType::MarkCancelled
    );
    let Some(instance) = store.get_instance(&record.workflow_id).await? else {
        anyhow::bail!("declarative workflow disappeared after completion");
    };
    assert_eq!(instance.state, "cancelled");
    Ok(())
}

#[tokio::test]
async fn declarative_submission_can_be_cancelled_by_an_operator() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_test_definition();
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let project_root = create_test_project(dir.path())?;
    let task_id = TaskId::from_str("declarative-operator-cancel");
    let record = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: &project_root,
            definition_id: TEST_DEFINITION_ID,
            task_id: &task_id,
            prompt: "cancel the declared work",
            depends_on: &[],
            serialization_depends_on: &[],
            source: Some("operator-test"),
            external_id: Some("cancel-1"),
        },
    )
    .await?;

    let outcome = cancel_submission_by_workflow_id(&store, &record.workflow_id).await?;
    let RuntimeSubmissionCancelOutcome::Cancelled(instance) = outcome else {
        anyhow::bail!("declarative submission did not report a cancelled outcome");
    };
    assert_eq!(instance.state, "cancelled");
    assert_eq!(instance.data["cancelled"], true);
    assert_eq!(
        instance.data["last_decision"],
        "cancel_declarative_submission"
    );
    Ok(())
}

#[tokio::test]
async fn declarative_submission_rejects_dependencies() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    register_test_definition();
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let task_id = TaskId::from_str("declarative-dependent");
    let dependency = TaskId::from_str("unsupported-dependency");

    let error = record_declarative_submission(
        &store,
        DeclarativeSubmissionRuntimeContext {
            project_root: dir.path(),
            definition_id: TEST_DEFINITION_ID,
            task_id: &task_id,
            prompt: "perform the declared work",
            depends_on: &[dependency],
            serialization_depends_on: &[],
            source: None,
            external_id: None,
        },
    )
    .await
    .expect_err("declarative dependencies must be rejected");

    assert!(error.to_string().contains("does not support dependencies"));
    Ok(())
}

#[tokio::test]
async fn declarative_submission_rejects_unknown_and_builtin_ids() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = open_declarative_runtime_store(dir.path()).await?;
    let task_id = TaskId::from_str("declarative-invalid-id");

    for definition_id in ["missing_definition", PROMPT_TASK_DEFINITION_ID] {
        let error = record_declarative_submission(
            &store,
            DeclarativeSubmissionRuntimeContext {
                project_root: dir.path(),
                definition_id,
                task_id: &task_id,
                prompt: "perform the declared work",
                depends_on: &[],
                serialization_depends_on: &[],
                source: None,
                external_id: None,
            },
        )
        .await
        .expect_err("non-declarative definitions must be rejected");
        assert!(
            error
                .to_string()
                .contains("is not a registered declarative definition"),
            "unexpected error for {definition_id}: {error}"
        );
    }
    Ok(())
}
