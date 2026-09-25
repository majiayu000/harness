use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

pub(crate) async fn build_workspace_pool_config(
    server: &HarnessServer,
    project_registry: Option<&Arc<crate::project_registry::ProjectRegistry>>,
) -> anyhow::Result<crate::workspace_pool::WorkspacePoolConfig> {
    let mut per_project = configured_project_limits(server);

    if let Some(registry) = project_registry {
        for project in registry.list().await? {
            if !project.active {
                continue;
            }
            let Some(limit) = project.max_concurrent else {
                continue;
            };
            per_project.insert(
                crate::workspace_pool::project_limit_key(&project.root),
                (limit as usize).max(1),
            );
        }
    }

    Ok(workspace_pool_config_from_limits(server, per_project))
}

fn configured_project_limits(server: &HarnessServer) -> std::collections::HashMap<String, usize> {
    server
        .config
        .concurrency
        .per_project
        .iter()
        .map(|(project, limit)| {
            (
                crate::workspace_pool::project_limit_key(Path::new(project)),
                (*limit).max(1),
            )
        })
        .collect()
}

fn workspace_pool_config_from_limits(
    server: &HarnessServer,
    mut per_project: std::collections::HashMap<String, usize>,
) -> crate::workspace_pool::WorkspacePoolConfig {
    for project in &server.startup_projects {
        let Some(limit) = project.max_concurrent else {
            continue;
        };
        per_project.insert(
            crate::workspace_pool::project_limit_key(&project.root),
            (limit as usize).max(1),
        );
    }

    crate::workspace_pool::WorkspacePoolConfig::new(
        server.config.concurrency.max_concurrent_tasks,
        per_project,
    )
}
