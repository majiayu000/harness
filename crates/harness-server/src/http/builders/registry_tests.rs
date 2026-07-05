use super::*;
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;

async fn make_test_server_and_tasks(
    dir: &Path,
) -> Option<(Arc<HarnessServer>, Arc<crate::task_runner::TaskStore>)> {
    if !crate::test_helpers::db_tests_enabled().await {
        return None;
    }
    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let tasks = crate::task_runner::TaskStore::open(&harness_core::config::dirs::default_db_path(
        dir, "tasks",
    ))
    .await
    .expect("open tasks db");
    Some((server, tasks))
}

#[tokio::test]
async fn empty_data_dir_produces_empty_project_registry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };
    let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
        .await
        .expect("build_registry should succeed");
    let project_registry = bundle.project_registry.as_ref().unwrap_or_else(|| {
        panic!(
            "project registry should be ready: {:?}",
            bundle.startup_results
        )
    });
    let projects = project_registry.list().await.expect("list projects");
    assert_eq!(projects.len(), 1, "expected only the default project");
    let canonical = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|_| dir.path().to_path_buf());
    assert_eq!(projects[0].id, canonical.to_string_lossy().as_ref());
}

#[tokio::test]
async fn plan_cache_hydrated_from_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };

    let plan_db = crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(
        dir.path(),
        "plans",
    ))
    .await
    .expect("open plan db");
    let plan =
        harness_exec::plan::ExecPlan::from_spec("# test-plan-id", dir.path()).expect("build plan");
    plan_db.upsert(&plan).await.expect("upsert plan");
    let plan_id = plan.id.as_str().to_string();

    let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
        .await
        .expect("build_registry");
    assert!(
        bundle.plan_cache.contains_key(&plan_id),
        "plan should be hydrated into cache"
    );
}

#[tokio::test]
async fn build_registry_opens_thread_db_from_shared_schema() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };
    let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
        .await
        .expect("build_registry should succeed");
    let thread_db = bundle
        .thread_db
        .as_ref()
        .expect("thread db should be ready");
    assert_eq!(thread_db.schema(), crate::thread_db::THREAD_DB_SCHEMA);
}

#[tokio::test]
async fn build_registry_opens_project_registry_from_shared_schema() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };
    let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
        .await
        .expect("build_registry should succeed");
    let project_registry = bundle
        .project_registry
        .as_ref()
        .expect("project registry should be ready");
    assert_eq!(
        project_registry.schema(),
        crate::project_registry::PROJECT_REGISTRY_SCHEMA
    );
}

#[tokio::test]
async fn runtime_state_failure_is_recorded_as_optional() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };
    let bundle = super::super::with_forced_startup_failures(
        &[(
            "runtime_state_store",
            "pool timed out while waiting for an open connection",
        )],
        build_registry(&server, dir.path(), dir.path(), &tasks),
    )
    .await
    .expect("build_registry should succeed");
    assert!(
        bundle.project_registry.is_some(),
        "critical stores should stay ready: {:?}",
        bundle.startup_results
    );
    assert!(
        bundle.runtime_state_store.is_none(),
        "optional runtime state store should be disabled"
    );
    let status = bundle
        .startup_results
        .iter()
        .find(|status| status.name == "runtime_state_store")
        .expect("runtime_state_store startup result");
    assert!(!status.is_critical());
    assert!(!status.ready);
}

#[tokio::test]
async fn invalid_workflow_schema_namespace_disables_optional_workflow_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nstorage:\n  schema_namespace: invalid-name\n---\n",
    )
    .expect("write workflow config");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };

    let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
        .await
        .expect("optional workflow-store context failures should not abort startup");

    assert!(
        bundle.project_registry.is_some(),
        "critical stores should stay ready: {:?}",
        bundle.startup_results
    );
    assert!(bundle.issue_workflow_store.is_none());
    assert!(bundle.project_workflow_store.is_none());
    for name in ["issue_workflow_store", "project_workflow_store"] {
        let status = bundle
            .startup_results
            .iter()
            .find(|status| status.name == name)
            .unwrap_or_else(|| panic!("{name} startup result should be recorded"));
        assert!(!status.is_critical());
        assert!(!status.ready);
    }
}

#[test]
fn bootstrap_failure_records_workspace_manager_status() {
    let statuses =
        super::super::registry_failures::failed_registry_startup_results("database unavailable");
    let workspace_status = statuses
        .iter()
        .find(|status| status.name == "workspace_manager")
        .expect("workspace_manager status");

    assert!(!workspace_status.ready);
    assert!(!workspace_status.is_critical());
}

#[tokio::test]
async fn project_registry_failure_is_recorded_as_critical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some((server, tasks)) = make_test_server_and_tasks(dir.path()).await else {
        return;
    };
    let bundle = super::super::with_forced_startup_failures(
        &[("project_registry", "failed to open Postgres bootstrap pool")],
        build_registry(&server, dir.path(), dir.path(), &tasks),
    )
    .await
    .expect("build_registry should succeed");
    assert!(
        bundle.project_registry.is_none(),
        "critical project registry should be absent"
    );
    let status = bundle
        .startup_results
        .iter()
        .find(|status| status.name == "project_registry")
        .expect("project_registry startup result");
    assert!(status.is_critical());
    assert!(!status.ready);
}
