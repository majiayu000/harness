use super::output::rewrite_eval_report;
use anyhow::Context;
use clap::Args;
use harness_core::config::HarnessConfig;
use harness_observe::event_store::EventStore;
use harness_workflow::runtime::{retry_eval_report_events, EvalRunReport};
use std::fs;
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct EvalRetryEventsArgs {
    /// Eval report whose deterministic observe events were not persisted
    pub(super) report: PathBuf,
}

pub(super) async fn retry_eval_events(
    args: EvalRetryEventsArgs,
    config: &HarnessConfig,
) -> anyhow::Result<()> {
    if config.server.database_url.is_none() {
        anyhow::bail!("retry-events requires server.database_url or HARNESS_DATABASE_URL");
    }
    let payload = fs::read_to_string(&args.report)
        .with_context(|| format!("failed to read eval report at {}", args.report.display()))?;
    let report: EvalRunReport = serde_json::from_str(&payload)
        .with_context(|| format!("invalid eval report {}", args.report.display()))?;
    if !report
        .outcome
        .is_some_and(|outcome| outcome.has_event_persistence_failure())
    {
        anyhow::bail!(
            "eval report {} does not have an event persistence failure",
            args.report.display()
        );
    }

    let observe = EventStore::with_policies_and_otel_with_database_url(
        &config.server.data_dir,
        config.server.database_url.as_deref(),
        config.observe.session_renewal_secs,
        config.observe.log_retention_days,
        &config.otel,
    )
    .await
    .context("failed to open observe event store for eval event retry")?;
    let recovered = retry_eval_report_events(&observe, &report).await;
    observe.shutdown().await;
    let recovered = recovered.context("failed to retry required eval outcome events")?;
    rewrite_eval_report(&args.report, &recovered)?;
    println!(
        "retried eval outcome events and repaired {}",
        args.report.display()
    );
    Ok(())
}
