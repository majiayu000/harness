use super::*;

/// Spawn the periodic reconciliation loop as a background task.
///
/// Returns immediately without spawning when `config.enabled` is false.
pub fn start(state: Arc<AppState>, config: ReconciliationConfig) {
    if !config.enabled {
        tracing::debug!("reconciliation: periodic loop disabled");
        return;
    }

    tokio::spawn(async move {
        reconciliation_loop(state, config).await;
    });
}

async fn reconciliation_loop(state: Arc<AppState>, config: ReconciliationConfig) {
    let interval = Duration::from_secs(config.interval_secs);
    // Brief init delay so the server is fully up before the first tick.
    sleep(Duration::from_secs(15)).await;

    loop {
        let report = run_once_with_runtime_config(
            &state.core.tasks,
            state.core.workflow_runtime_store.as_deref(),
            state.core.issue_workflow_store.as_deref(),
            &config,
            false,
            state.core.server.config.server.github_token.as_deref(),
        )
        .await;
        record_repo_backlog_reconciliation_transitions(&state, &report).await;

        // Clean up workspaces for tasks that were just terminated by reconciliation.
        if let Some(ref wmgr) = state.concurrency.workspace_mgr {
            let transitioned_ids: Vec<crate::task_runner::TaskId> = report
                .transitions
                .iter()
                .filter(|t| t.applied)
                .map(|t| harness_core::types::TaskId(t.task_id.clone()))
                .collect();
            if !transitioned_ids.is_empty() {
                if let Err(e) = wmgr.cleanup_terminal(&transitioned_ids).await {
                    tracing::warn!("reconciliation: workspace cleanup failed: {e}");
                }
            }
        }

        tracing::info!(
            candidates = report.candidates,
            skipped_terminal = report.skipped_terminal,
            transitions = report.transitions.len(),
            workflow_transitions = report.workflow_transitions.len(),
            workflow_alerts = report.workflow_alerts.len(),
            "reconciliation: tick complete"
        );
        sleep(interval).await;
    }
}

async fn record_repo_backlog_reconciliation_transitions(
    state: &Arc<AppState>,
    report: &ReconciliationReport,
) {
    for transition in &report.transitions {
        if !transition.applied || transition.reason != "reconciled: PR merged externally" {
            continue;
        }
        let task_id = harness_core::types::TaskId(transition.task_id.clone());
        let Some(task) = state.core.tasks.get(&task_id) else {
            continue;
        };
        let Some(pr_number) = task
            .pr_url
            .as_deref()
            .and_then(harness_core::prompts::parse_github_pr_url)
            .map(|(_, _, pr_number)| pr_number)
            .or_else(|| parse_external_id(task.external_id.as_deref()).1)
        else {
            continue;
        };
        let issue_number = task
            .issue
            .or_else(|| parse_external_id(task.external_id.as_deref()).0);
        let project_root = task
            .project_root
            .as_deref()
            .unwrap_or(&state.core.project_root);
        crate::workflow_runtime_repo_backlog::record_merged_pr(
            state.core.workflow_runtime_store.as_deref(),
            state.core.issue_workflow_store.as_deref(),
            crate::workflow_runtime_repo_backlog::MergedPrRuntimeContext {
                project_root,
                repo: task.repo.as_deref(),
                issue_number,
                pr_number,
                pr_url: task.pr_url.as_deref(),
                detail: &transition.reason,
            },
        )
        .await;
    }
}
