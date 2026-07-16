//! GH-1652: nonterminal declarative workflow instances must appear in the
//! dashboard and overview active counts (B-003), and a registered definition
//! with zero instances must contribute nothing (B-005).
//!
//! The Postgres-backed workflow runtime schema is shared across test runs, so
//! assertions are scoped to this test's unique project id (the same pattern
//! the dashboard handler tests use) instead of global totals.

use crate::http::{build_app_state, AppState};
use crate::server::HarnessServer;
use crate::test_helpers;
use crate::thread_manager::ThreadManager;
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_workflow::runtime::{WorkflowInstance, WorkflowSubject};
use std::sync::Arc;

const COUNTS_FIXTURE_DEFINITION_ID: &str = "active_counts_declarative_fixture";
const EMPTY_COUNTS_FIXTURE_DEFINITION_ID: &str = "active_counts_declarative_empty_fixture";

async fn make_counts_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let mut config = HarnessConfig::default();
    config.server.project_root = dir.to_path_buf();
    config.server.data_dir = dir.to_path_buf();
    config.server.allow_unauthenticated = true;
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    build_app_state(server).await
}

fn blocked_declarative_workflow(id: &str, project_id: &str) -> WorkflowInstance {
    let definition =
        test_helpers::register_declarative_fixture_definition(COUNTS_FIXTURE_DEFINITION_ID);
    WorkflowInstance::new(
        COUNTS_FIXTURE_DEFINITION_ID,
        definition.definition_version(),
        "blocked",
        WorkflowSubject::new("prompt", "prompt:gh1652-counts"),
    )
    .with_data(serde_json::json!({
        "definition_hash": definition.definition_hash(),
        "source": "declarative",
        "project_id": project_id,
    }))
    .with_id(id.to_string())
}

#[tokio::test]
async fn blocked_declarative_workflow_counts_in_dashboard_and_overview() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = test_helpers::tempdir_in_home("harness-test-declarative-active-counts-")?;
    let project_id = dir.path().canonicalize()?.to_string_lossy().into_owned();
    let state = make_counts_state(dir.path()).await?;
    test_helpers::register_declarative_fixture_definition(EMPTY_COUNTS_FIXTURE_DEFINITION_ID);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("test state should open a workflow runtime store");

    // B-005: registered declarative definitions with zero instances
    // contribute nothing to either surface for this project.
    let dashboard_before =
        super::dashboard_active_counts::dashboard_active_counts(&state, None).await;
    assert!(
        !dashboard_before.by_project.contains_key(&project_id),
        "zero-instance declarative definitions must contribute no dashboard counts"
    );
    let overview_before = super::overview::active_task_overview_counts(&state).await;
    assert!(
        !overview_before.by_project.contains_key(&project_id),
        "zero-instance declarative definitions must contribute no overview counts"
    );

    store
        .upsert_instance(&blocked_declarative_workflow(
            "declarative-blocked-counts",
            &project_id,
        ))
        .await?;

    // B-003: blocked nonterminal declarative instances project into the
    // queued active bucket on both surfaces.
    let dashboard_after =
        super::dashboard_active_counts::dashboard_active_counts(&state, None).await;
    let dashboard_project = dashboard_after
        .by_project
        .get(&project_id)
        .copied()
        .expect("blocked declarative workflow must count in dashboard active counts");
    assert_eq!(dashboard_project.running, 0);
    assert_eq!(dashboard_project.queued, 1);

    let overview_after = super::overview::active_task_overview_counts(&state).await;
    let overview_project = overview_after
        .by_project
        .get(&project_id)
        .copied()
        .expect("blocked declarative workflow must count in overview active counts");
    assert_eq!(overview_project.running, 0);
    assert_eq!(overview_project.queued, 1);
    Ok(())
}
