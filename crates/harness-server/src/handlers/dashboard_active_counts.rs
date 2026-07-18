use crate::{
    http::AppState,
    project_registry::check_allowed_roots,
    runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct DashboardProjectActiveCounts {
    pub(super) running: usize,
    pub(super) queued: usize,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DashboardActiveCounts {
    pub(super) running: usize,
    pub(super) queued: usize,
    pub(super) by_project: HashMap<String, DashboardProjectActiveCounts>,
    pub(super) used_task_queue_fallback: bool,
}

impl DashboardActiveCounts {
    fn add(&mut self, project_id: Option<&str>, bucket: DashboardActiveBucket) {
        let project_counts =
            project_id.map(|project_id| self.by_project.entry(project_id.to_owned()).or_default());
        match bucket {
            DashboardActiveBucket::Running => {
                self.running += 1;
                if let Some(entry) = project_counts {
                    entry.running += 1;
                }
            }
            DashboardActiveBucket::Queued => {
                self.queued += 1;
                if let Some(entry) = project_counts {
                    entry.queued += 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardActiveBucket {
    Running,
    Queued,
}

pub(super) async fn dashboard_active_counts(
    state: &AppState,
    visible_project_ids: Option<&HashSet<String>>,
) -> DashboardActiveCounts {
    let mut counts = DashboardActiveCounts::default();
    let runtime_counts_available = state.core.workflow_runtime_store.is_some();
    let scope_root = &state.core.project_root;
    let allowed_project_roots = &state.core.server.config.server.allowed_project_roots;

    if let Some(store) = state.core.workflow_runtime_store.as_ref() {
        match crate::handlers::definition_ids::active_count_definition_ids() {
            Ok(definition_ids) => {
                for definition_id in &definition_ids {
                    match store
                        .list_nonterminal_instances_by_definition(definition_id, None, None)
                        .await
                    {
                        Ok(workflows) => {
                            for workflow in workflows {
                                let projection =
                                    RuntimeWorkflowProjection::from_workflow(&workflow);
                                add_active_runtime_workflow(
                                    &mut counts,
                                    &projection,
                                    visible_project_ids,
                                    scope_root,
                                    allowed_project_roots,
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                definition_id = definition_id.as_str(),
                                "dashboard: failed to list runtime workflows for active counts: {error}"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!("dashboard: {error}; runtime workflow active counts unavailable");
            }
        }
    }

    if !runtime_counts_available {
        counts.running = state.concurrency.task_queue.running_count();
        counts.queued = state.concurrency.task_queue.queued_count();
        counts.used_task_queue_fallback = true;
    }

    counts
}

fn add_active_runtime_workflow(
    counts: &mut DashboardActiveCounts,
    projection: &RuntimeWorkflowProjection,
    visible_project_ids: Option<&HashSet<String>>,
    scope_root: &Path,
    allowed_project_roots: &[std::path::PathBuf],
) -> bool {
    if !project_is_in_dashboard_scope(
        projection.project_id.as_deref(),
        visible_project_ids,
        scope_root,
        allowed_project_roots,
    ) {
        return false;
    }
    let Some(bucket) = projection.active_bucket() else {
        return false;
    };
    let bucket = match bucket {
        RuntimeActiveBucket::Running => DashboardActiveBucket::Running,
        RuntimeActiveBucket::Queued => DashboardActiveBucket::Queued,
    };
    let visible_project_id = projection
        .project_id
        .as_deref()
        .filter(|project_id| project_is_visible(Some(project_id), visible_project_ids));
    counts.add(visible_project_id, bucket);
    true
}

fn project_is_in_dashboard_scope(
    project_id: Option<&str>,
    visible_project_ids: Option<&HashSet<String>>,
    scope_root: &Path,
    allowed_project_roots: &[std::path::PathBuf],
) -> bool {
    let Some(project_id) = project_id else {
        return true;
    };
    let project_path = Path::new(project_id);
    project_is_visible(Some(project_id), visible_project_ids)
        || project_path.starts_with(scope_root)
        || (project_path.is_dir()
            && check_allowed_roots(project_path, allowed_project_roots).is_ok())
}

fn project_is_visible(
    project_id: Option<&str>,
    visible_project_ids: Option<&HashSet<String>>,
) -> bool {
    visible_project_ids.is_none_or(|visible_project_ids| {
        project_id.is_none_or(|project_id| visible_project_ids.contains(project_id))
    })
}
