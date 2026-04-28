use anyhow::Result;
use harness_core::config::dirs::default_db_path;
use harness_core::config::HarnessConfig;
use std::path::PathBuf;

pub async fn run(dry_run: bool, _project: Option<PathBuf>, config: &HarnessConfig) -> Result<()> {
    let db_path = default_db_path(&config.server.data_dir, "tasks");
    let store = harness_server::task_runner::TaskStore::open(&db_path).await?;

    let report = harness_server::reconciliation::run_once_with_token(
        &store,
        config.reconciliation.max_gh_calls_per_minute,
        dry_run,
        config.server.github_token.as_deref(),
    )
    .await;

    if dry_run {
        println!(
            "Reconciliation dry-run: {} candidate(s), {} terminal skipped",
            report.candidates, report.skipped_terminal
        );
    } else {
        println!(
            "Reconciliation: {} candidate(s), {} terminal skipped, {} transition(s) applied",
            report.candidates,
            report.skipped_terminal,
            report.transitions.len()
        );
    }

    for t in &report.transitions {
        let applied = if t.applied { "applied" } else { "dry-run" };
        println!("  {} → {} ({}) [{}]", t.from, t.to, t.reason, applied);
    }

    Ok(())
}
