use dashmap::DashMap;
use std::sync::Arc;

use crate::http::builders::registry::RegistryBundle;
use crate::http::state::StoreStartupResult;

pub(crate) fn failed_registry_startup_results(error: &str) -> Vec<StoreStartupResult> {
    vec![
        StoreStartupResult::critical("plan_db").failed(error),
        StoreStartupResult::optional("issue_workflow_store").failed(error),
        StoreStartupResult::optional("project_workflow_store").failed(error),
        StoreStartupResult::optional("workflow_runtime_store").failed(error),
        StoreStartupResult::critical("project_registry").failed(error),
        StoreStartupResult::optional("workspace_lease_store").failed(error),
        StoreStartupResult::optional("workspace_manager").failed(error),
        StoreStartupResult::optional("runtime_state_store").failed(error),
    ]
}

pub(crate) fn failed_registry_bundle(
    plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    error: &str,
) -> RegistryBundle {
    RegistryBundle {
        plan_db: None,
        plan_cache,
        issue_workflow_store: None,
        project_workflow_store: None,
        workflow_runtime_store: None,
        project_registry: None,
        runtime_state_store: None,
        workspace_mgr: None,
        startup_results: failed_registry_startup_results(error),
    }
}
