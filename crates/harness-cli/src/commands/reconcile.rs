use anyhow::{bail, Result};
use harness_core::config::dirs::default_db_path;
use harness_core::config::HarnessConfig;
use harness_workflow::issue_lifecycle::IssueWorkflowStore;
use harness_workflow::runtime::WorkflowRuntimeStore;
use std::path::PathBuf;

pub async fn run(dry_run: bool, project: Option<PathBuf>, config: &HarnessConfig) -> Result<()> {
    if let Some(project_root) = project {
        bail!(
            "`harness reconcile --project {}` is no longer supported; reconciliation now uses each task's stored project root",
            project_root.display()
        );
    }

    let db_path = default_db_path(&config.server.data_dir, "tasks");
    let store = harness_server::task_runner::TaskStore::open(&db_path).await?;
    let (runtime_store, issue_workflow_store) = open_workflow_stores(config).await;

    let report = harness_server::reconciliation::run_once_with_runtime_token(
        &store,
        runtime_store.as_ref(),
        issue_workflow_store.as_ref(),
        config.reconciliation.max_gh_calls_per_minute,
        dry_run,
        config.server.github_token.as_deref(),
    )
    .await;

    if dry_run {
        println!(
            "Reconciliation dry-run: {} candidate(s), {} terminal skipped, {} task transition(s), {} workflow transition(s)",
            report.candidates,
            report.skipped_terminal,
            report.transitions.len(),
            report.workflow_transitions.len()
        );
    } else {
        let applied = report.transitions.iter().filter(|t| t.applied).count()
            + report
                .workflow_transitions
                .iter()
                .filter(|t| t.applied)
                .count();
        println!(
            "Reconciliation: {} candidate(s), {} terminal skipped, {} transition(s) applied",
            report.candidates, report.skipped_terminal, applied
        );
    }

    for t in &report.transitions {
        let applied = if t.applied { "applied" } else { "dry-run" };
        println!("  {} → {} ({}) [{}]", t.from, t.to, t.reason, applied);
    }

    for t in &report.workflow_transitions {
        let applied = if t.applied { "applied" } else { "dry-run" };
        let repo = t.repo.as_deref().unwrap_or("<unknown>");
        println!(
            "  workflow {} {}#{}: {} → {} ({}) [{}]",
            t.workflow_id, repo, t.pr_number, t.from, t.to, t.reason, applied
        );
    }

    Ok(())
}

async fn open_workflow_stores(
    config: &HarnessConfig,
) -> (Option<WorkflowRuntimeStore>, Option<IssueWorkflowStore>) {
    let workflow_config =
        harness_core::config::workflow::load_workflow_config(&config.server.project_root)
            .unwrap_or_default();
    let workflow_ns = workflow_config.storage.schema_namespace;
    let runtime_schema = format!("{workflow_ns}_runtime");
    let issue_schema = format!("{workflow_ns}_issue");
    let database_url = config.server.database_url.as_deref();

    let runtime_store = match WorkflowRuntimeStore::open_with_database_url_and_schema(
        database_url,
        &runtime_schema,
    )
    .await
    {
        Ok(store) => Some(store),
        Err(error) => {
            tracing::warn!(
                schema = %runtime_schema,
                "workflow runtime store unavailable during reconciliation: {error}"
            );
            None
        }
    };
    let issue_workflow_store =
        match IssueWorkflowStore::open_with_database_url_and_schema(database_url, &issue_schema)
            .await
        {
            Ok(store) => Some(store),
            Err(error) => {
                tracing::warn!(
                    schema = %issue_schema,
                    "issue workflow store unavailable during reconciliation: {error}"
                );
                None
            }
        };

    (runtime_store, issue_workflow_store)
}
