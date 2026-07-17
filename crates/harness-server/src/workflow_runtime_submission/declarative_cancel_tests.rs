use super::*;
use harness_core::{
    config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    },
    db::resolve_database_url,
};
use harness_workflow::runtime::{
    build_declarative_definition, persisted_declarative_definition,
    register_declarative_workflow_definitions,
};
use std::{collections::BTreeMap, sync::Once};

const DEFINITION_ID: &str = "missing_pin_cancel_test_declarative";
static REGISTER_CURRENT_DEFINITION: Once = Once::new();

fn definition(with_stop_signal: bool) -> harness_workflow::runtime::DeclarativeWorkflowDefinition {
    let on_signal = if with_stop_signal {
        BTreeMap::from([("stop".to_string(), "withdrawn".to_string())])
    } else {
        BTreeMap::new()
    };
    build_declarative_definition(
        &WorkflowDefinitionPolicy {
            id: DEFINITION_ID.to_string(),
            initial: "working".to_string(),
            states: BTreeMap::from([
                (
                    "working".to_string(),
                    DeclaredState {
                        activity: Some("perform_work".to_string()),
                        on_success: Some("completed".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_signal,
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
                ("completed".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("withdrawn".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["working".to_string()],
            intake: None,
        },
        &BTreeMap::from([(
            "perform_work".to_string(),
            WorkflowActivityPolicy::default(),
        )]),
    )
    .expect("missing-pin cancellation fixture should compile")
}

#[tokio::test]
async fn missing_registered_pin_can_use_persisted_definition_for_cancellation() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let persisted_version = definition(false);
    REGISTER_CURRENT_DEFINITION.call_once(|| {
        register_declarative_workflow_definitions([definition(true)])
            .expect("current cancellation fixture should register");
    });
    let dir = tempfile::tempdir()?;
    let database_url = resolve_database_url(None)?;
    let store =
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
    store
        .persist_definition_version(&persisted_declarative_definition(
            &persisted_version,
            Some("/repo/WORKFLOW.md"),
        ))
        .await?;
    let instance = WorkflowInstance::new(
        DEFINITION_ID,
        persisted_version.definition_version(),
        "working",
        WorkflowSubject::new("declarative", "missing-pin-cancel"),
    )
    .with_id("missing-pin-cancel-workflow")
    .with_data(json!({
        "definition_hash": persisted_version.definition_hash(),
        "prompt_ref": "missing-pin-cancel-prompt",
    }));
    store.upsert_instance(&instance).await?;

    let outcome = cancel_submission_by_workflow_id(&store, &instance.id).await?;
    let RuntimeSubmissionCancelOutcome::Cancelled(cancelled) = outcome else {
        anyhow::bail!("missing-pin cancellation did not report a cancelled outcome");
    };
    assert_eq!(cancelled.state, "withdrawn");
    assert_eq!(cancelled.data["cancelled"], true);
    assert_eq!(
        cancelled.data["last_decision"],
        "cancel_declarative_submission"
    );
    Ok(())
}
