use super::{configured_rule_engine, RuleCommand};
use harness_core::config::HarnessConfig;

pub async fn run(cmd: RuleCommand, config: &HarnessConfig) -> anyhow::Result<()> {
    match cmd {
        RuleCommand::Load { project } => {
            let mut engine = configured_rule_engine(config);
            engine.load(&project)?;
            println!("Loaded {} rules", engine.rules().len());
        }
        RuleCommand::Check { project, auto_fix } => {
            let mut engine = configured_rule_engine(config);
            engine.load(&project)?;
            let violations = engine.scan(&project).await?;
            // Persist rule scan results for observability/GC even when running via CLI.
            match harness_observe::event_store::EventStore::with_policies_and_otel_with_database_url(
                &config.server.data_dir,
                config.server.database_url.as_deref(),
                config.observe.session_renewal_secs,
                config.observe.log_retention_days,
                &config.otel,
            )
            .await
            {
                Ok(store) => {
                    store.persist_rule_scan(&project, &violations).await;
                    store.shutdown().await;
                }
                Err(e) => tracing::warn!(
                    "Failed to initialize event store, rule scan not persisted: {e}"
                ),
            }
            if violations.is_empty() {
                println!("No violations found");
            } else {
                for v in &violations {
                    println!(
                        "{:?} {}:{} [{}] {}",
                        v.severity,
                        v.file.display(),
                        v.line.unwrap_or(0),
                        v.rule_id,
                        v.message
                    );
                }
                if auto_fix {
                    let fixed = engine.apply_fixes(&violations, &project)?;
                    println!("Auto-fixed {fixed} file(s)");
                }
            }
        }
    }
    Ok(())
}
