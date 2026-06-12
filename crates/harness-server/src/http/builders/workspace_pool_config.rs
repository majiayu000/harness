use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

pub(crate) async fn build_workspace_pool_config(
    server: &HarnessServer,
    project_registry: Option<&Arc<crate::project_registry::ProjectRegistry>>,
) -> crate::workspace_pool::WorkspacePoolConfig {
    let mut per_project = server
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
        .collect::<std::collections::HashMap<_, _>>();

    if let Some(registry) = project_registry {
        match registry.list().await {
            Ok(projects) => {
                for project in projects {
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
            Err(error) => {
                tracing::warn!("workspace pool: failed to load registry project limits: {error}");
            }
        }
    }

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
