use crate::{
    http::AppState,
    project_registry::check_allowed_roots,
    runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection},
    task_runner::{SchedulerAuthorityState, TaskId, TaskSummary},
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

    fn add_queued_waiters(&mut self, project_id: Option<&str>, queued: usize) {
        if queued == 0 {
            return;
        }
        self.queued += queued;
        if let Some(project_id) = project_id {
            self.by_project
                .entry(project_id.to_owned())
                .or_default()
                .queued += queued;
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
    let mut active_legacy_task_ids: HashSet<TaskId> = HashSet::new();
    let mut represented_task_queue_waiters = HashMap::new();
    let mut loaded_legacy_tasks = false;
    let mut counted_runtime_active_workflows = false;
    let scope_root = &state.core.project_root;
    let allowed_project_roots = &state.core.server.config.server.allowed_project_roots;

    match state.core.tasks.list_active_summaries().await {
        Ok(summaries) => {
            loaded_legacy_tasks = true;
            for summary in summaries {
                if add_active_summary(
                    &mut counts,
                    &summary,
                    visible_project_ids,
                    scope_root,
                    allowed_project_roots,
                ) {
                    if summary.scheduler.authority_state == SchedulerAuthorityState::Queued {
                        if let Some(project_id) = summary.project.as_deref() {
                            *represented_task_queue_waiters
                                .entry(project_id.to_owned())
                                .or_insert(0) += 1;
                        }
                    }
                    active_legacy_task_ids.insert(summary.id);
                }
            }
        }
        Err(error) => {
            tracing::warn!("dashboard: failed to list task summaries for active counts: {error}");
        }
    }

    if let Some(store) = state.core.workflow_runtime_store.as_ref() {
        // Registry-driven enumeration (GH-1652): built-in child workflow
        // definitions are excluded by the helper; declarative ids included.
        match super::definition_ids::active_count_definition_ids() {
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
                                if projection
                                    .legacy_dedupe_task_handle
                                    .as_ref()
                                    .is_some_and(|task_id| active_legacy_task_ids.contains(task_id))
                                {
                                    continue;
                                }
                                counted_runtime_active_workflows |= add_active_runtime_workflow(
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
                // B-006: cannot happen after healthy startup (registry seeds
                // built-ins); surface loudly instead of silently rendering an
                // empty runtime contribution.
                tracing::error!(
                    "dashboard: failed to enumerate workflow definitions for active counts: {error}"
                );
            }
        }
    }

    if !loaded_legacy_tasks && !counted_runtime_active_workflows {
        counts.running = state.concurrency.task_queue.running_count();
        counts.queued = state.concurrency.task_queue.queued_count();
        counts.used_task_queue_fallback = true;
    } else {
        add_task_queue_waiters(
            &mut counts,
            state,
            visible_project_ids,
            scope_root,
            allowed_project_roots,
            &represented_task_queue_waiters,
        );
    }

    counts
}

fn add_task_queue_waiters(
    counts: &mut DashboardActiveCounts,
    state: &AppState,
    visible_project_ids: Option<&HashSet<String>>,
    scope_root: &Path,
    allowed_project_roots: &[std::path::PathBuf],
    represented_task_queue_waiters: &HashMap<String, usize>,
) {
    for (project_id, stats) in state.concurrency.task_queue.all_project_stats() {
        if !project_is_in_dashboard_scope(
            Some(&project_id),
            visible_project_ids,
            scope_root,
            allowed_project_roots,
        ) {
            continue;
        }
        let queued = stats.queued.saturating_sub(
            represented_task_queue_waiters
                .get(&project_id)
                .copied()
                .unwrap_or(0),
        );
        let visible_project_id =
            project_is_visible(Some(&project_id), visible_project_ids).then_some(project_id);
        counts.add_queued_waiters(visible_project_id.as_deref(), queued);
    }
}

fn add_active_summary(
    counts: &mut DashboardActiveCounts,
    summary: &TaskSummary,
    visible_project_ids: Option<&HashSet<String>>,
    scope_root: &Path,
    allowed_project_roots: &[std::path::PathBuf],
) -> bool {
    if summary.status.is_terminal() {
        return false;
    }
    if !project_is_in_dashboard_scope(
        summary.project.as_deref(),
        visible_project_ids,
        scope_root,
        allowed_project_roots,
    ) {
        return false;
    }
    let bucket = match summary.scheduler.authority_state {
        SchedulerAuthorityState::Running
        | SchedulerAuthorityState::Leased
        | SchedulerAuthorityState::Recovering => DashboardActiveBucket::Running,
        _ => DashboardActiveBucket::Queued,
    };
    let visible_project_id = summary
        .project
        .as_deref()
        .filter(|project_id| project_is_visible(Some(project_id), visible_project_ids));
    counts.add(visible_project_id, bucket);
    true
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
