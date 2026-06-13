use anyhow::{bail, Context as _, Result};
use harness_core::config::dirs::default_db_path;
use harness_core::config::HarnessConfig;
use harness_core::db::{pg_open_pool, resolve_database_url};
use harness_server::task_runner::TaskStore;
use harness_workflow::issue_lifecycle::IssueWorkflowStore;
use harness_workflow::runtime::WorkflowRuntimeStore;
use std::path::PathBuf;
use std::sync::Arc;

pub async fn run(dry_run: bool, project: Option<PathBuf>, config: &HarnessConfig) -> Result<()> {
    if let Some(project_root) = project {
        bail!(
            "`harness reconcile --project {}` is no longer supported; reconciliation now uses each task's stored project root",
            project_root.display()
        );
    }

    let store = open_task_store(config).await?;
    let (runtime_store, issue_workflow_store) = open_workflow_stores(config).await;

    let report = harness_server::reconciliation::run_once_with_runtime_config(
        &store,
        runtime_store.as_ref(),
        issue_workflow_store.as_ref(),
        &config.reconciliation,
        dry_run,
        config.server.github_token.as_deref(),
    )
    .await;

    if dry_run {
        println!(
            "Reconciliation dry-run: {} candidate(s), {} terminal skipped, {} task transition(s), {} workflow transition(s), {} workflow alert(s)",
            report.candidates,
            report.skipped_terminal,
            report.transitions.len(),
            report.workflow_transitions.len(),
            report.workflow_alerts.len()
        );
    } else {
        let applied = report.transitions.iter().filter(|t| t.applied).count()
            + report
                .workflow_transitions
                .iter()
                .filter(|t| t.applied)
                .count();
        println!(
            "Reconciliation: {} candidate(s), {} terminal skipped, {} transition(s) applied, {} workflow alert(s)",
            report.candidates,
            report.skipped_terminal,
            applied,
            report.workflow_alerts.len()
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

    for alert in &report.workflow_alerts {
        let repo = alert.repo.as_deref().unwrap_or("<unknown>");
        let issue_number = alert
            .issue_number
            .map(|number| number.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let pr_url = alert.pr_url.as_deref().unwrap_or("<unknown>");
        println!(
            "  workflow alert {} repo={} issue=#{} pr=#{} state={} age_secs={} ttl_secs={} reason={} url={}",
            alert.workflow_id,
            repo,
            issue_number,
            alert.pr_number,
            alert.state,
            alert.age_secs,
            alert.ttl_secs,
            alert.reason,
            pr_url
        );
    }

    Ok(())
}

async fn open_task_store(config: &HarnessConfig) -> Result<Arc<TaskStore>> {
    std::fs::create_dir_all(&config.server.data_dir).with_context(|| {
        format!(
            "failed to create data_dir {}",
            config.server.data_dir.display()
        )
    })?;
    let data_dir = config.server.data_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize data_dir {}",
            config.server.data_dir.display()
        )
    })?;
    let db_path = default_db_path(&data_dir, "tasks");
    let database_url = resolve_database_url(config.server.database_url.as_deref())?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let task_context = harness_server::task_db::TaskDb::shared_schema_context(Some(&database_url))?;
    TaskStore::open_shared_for_reconciliation_with_data_dir(
        &db_path,
        &task_context,
        &setup_pool,
        &data_dir,
        Some(&database_url),
    )
    .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::TaskId;
    use harness_server::task_db::TaskDb;
    use harness_server::task_runner::{
        TaskKind, TaskPhase, TaskSchedulerState, TaskState, TaskStatus,
    };

    #[tokio::test]
    async fn open_task_store_reads_shared_schema_scoped_to_data_dir() -> Result<()> {
        let database_url = match resolve_database_url(None) {
            Ok(database_url) => database_url,
            Err(_) => return Ok(()),
        };
        let setup_pool = match pg_open_pool(&database_url).await {
            Ok(pool) => pool,
            Err(_) => return Ok(()),
        };
        let sandbox = tempfile::tempdir()?;
        let target_data_dir = sandbox.path().join("target-data");
        let other_data_dir = sandbox.path().join("other-data");
        std::fs::create_dir_all(&target_data_dir)?;
        std::fs::create_dir_all(&other_data_dir)?;

        let task_context =
            harness_server::task_db::TaskDb::shared_schema_context(Some(&database_url))?;
        let target_db =
            TaskDb::open_shared_with_data_dir(&task_context, &setup_pool, &target_data_dir).await?;
        let other_db =
            TaskDb::open_shared_with_data_dir(&task_context, &setup_pool, &other_data_dir).await?;
        let target_id = TaskId("reconcile-shared-target".to_string());
        let other_id = TaskId("reconcile-shared-other".to_string());
        let running_id = TaskId("reconcile-shared-running".to_string());
        target_db
            .insert(&task_state(
                &target_id,
                &target_data_dir,
                TaskStatus::Pending,
            ))
            .await?;
        other_db
            .insert(&task_state(&other_id, &other_data_dir, TaskStatus::Pending))
            .await?;
        target_db
            .insert(&task_state(
                &running_id,
                &target_data_dir,
                TaskStatus::Implementing,
            ))
            .await?;

        let mut config = HarnessConfig::default();
        config.server.data_dir = target_data_dir;
        config.server.database_url = Some(database_url);

        let store = open_task_store(&config).await?;
        assert!(
            store.get_with_db_fallback(&target_id).await?.is_some(),
            "reconcile task store should see tasks from its configured data_dir"
        );
        assert!(
            store.get_with_db_fallback(&other_id).await?.is_none(),
            "reconcile task store must not see tasks from another data_dir"
        );
        let running_task = store
            .get_with_db_fallback(&running_id)
            .await?
            .expect("reconcile task store should load active running tasks");
        assert_eq!(
            running_task.status,
            TaskStatus::Implementing,
            "reconcile task store must not run startup recovery side effects"
        );
        let persisted_running = target_db
            .get(running_id.as_str())
            .await?
            .expect("running task should remain persisted");
        assert_eq!(
            persisted_running.status,
            TaskStatus::Implementing,
            "opening reconcile task store must not fail running tasks in storage"
        );

        Ok(())
    }

    fn task_state(id: &TaskId, project_root: &std::path::Path, status: TaskStatus) -> TaskState {
        TaskState {
            id: id.clone(),
            task_kind: TaskKind::Prompt,
            status,
            failure_kind: None,
            turn: 0,
            pr_url: None,
            rounds: Vec::new(),
            error: None,
            source: Some("test".to_string()),
            external_id: None,
            parent_id: None,
            depends_on: Vec::new(),
            subtask_ids: Vec::new(),
            project_root: Some(project_root.to_path_buf()),
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            issue: None,
            repo: None,
            description: Some("reconcile shared schema test task".to_string()),
            created_at: None,
            updated_at: None,
            priority: 0,
            phase: TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
            scheduler: TaskSchedulerState::queued(),
            version: 0,
        }
    }
}
