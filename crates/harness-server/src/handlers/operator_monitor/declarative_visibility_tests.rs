//! GH-1652: declarative workflow definitions must be as visible to the
//! operator monitor as built-ins (B-003), and a registered definition with
//! zero instances must contribute nothing (B-005).

use super::*;
use crate::test_helpers;
use harness_workflow::runtime::{WorkflowRuntimeStore, WorkflowSubject};

const BLOCKED_FIXTURE_DEFINITION_ID: &str = "operator_monitor_declarative_fixture";
const EMPTY_FIXTURE_DEFINITION_ID: &str = "operator_monitor_declarative_empty_fixture";

fn blocked_declarative_workflow(id: &str) -> WorkflowInstance {
    let definition =
        test_helpers::register_declarative_fixture_definition(BLOCKED_FIXTURE_DEFINITION_ID);
    WorkflowInstance::new(
        BLOCKED_FIXTURE_DEFINITION_ID,
        definition.definition_version(),
        "blocked",
        WorkflowSubject::new("prompt", "prompt:gh1652"),
    )
    .with_data(json!({
        "definition_hash": definition.definition_hash(),
        "source": "declarative",
    }))
    .with_id(id.to_string())
}

async fn open_store(dir: &std::path::Path) -> anyhow::Result<WorkflowRuntimeStore> {
    WorkflowRuntimeStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir, "workflow_runtime"),
        Some(&test_helpers::test_database_url()?),
    )
    .await
}

#[tokio::test]
async fn blocked_declarative_workflow_appears_in_monitor_sample_and_actions() -> anyhow::Result<()>
{
    let _lock = test_helpers::HOME_LOCK.lock().await;
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-declarative-")?;
    let store = open_store(dir.path()).await?;
    test_helpers::register_declarative_fixture_definition(EMPTY_FIXTURE_DEFINITION_ID);
    store
        .upsert_instance(&blocked_declarative_workflow("declarative-blocked-1"))
        .await?;

    let workflows = list_runtime_workflows_from_store(&store).await?;

    let sampled = workflows
        .iter()
        .find(|workflow| workflow.id == "declarative-blocked-1")
        .expect("blocked declarative workflow should be sampled by the monitor");
    assert_eq!(sampled.definition_id, BLOCKED_FIXTURE_DEFINITION_ID);
    assert_eq!(sampled.state, "blocked");

    let actions = operator_actions(&workflows, Utc::now(), &HashMap::new());
    let action = actions
        .iter()
        .find(|action| action.workflow_id == "declarative-blocked-1")
        .expect("blocked declarative workflow should surface as an operator action");
    assert_eq!(action.kind, "blocked");
    assert_eq!(action.next_action, "Resolve blocker");
    assert_eq!(action.source, "declarative");

    // B-005: a registered declarative definition with zero instances
    // contributes no monitor rows.
    assert!(
        workflows
            .iter()
            .all(|workflow| workflow.definition_id != EMPTY_FIXTURE_DEFINITION_ID),
        "zero-instance declarative definition must contribute nothing"
    );
    Ok(())
}

#[tokio::test]
async fn declarative_sampling_uses_registry_definition_ids() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-declarative-sample-")?;
    let store = open_store(dir.path()).await?;
    store
        .upsert_instance(&blocked_declarative_workflow("declarative-blocked-sampled"))
        .await?;

    let definition_ids = crate::handlers::definition_ids::operator_definition_ids()?;
    assert!(
        definition_ids
            .iter()
            .any(|id| id == BLOCKED_FIXTURE_DEFINITION_ID),
        "registry enumeration must include the declarative fixture"
    );

    let workflows = sampling::list_operator_action_workflows(&store, &definition_ids).await?;
    assert!(
        workflows
            .iter()
            .any(|workflow| workflow.id == "declarative-blocked-sampled"),
        "operator-action sampling must include blocked declarative instances"
    );
    Ok(())
}
