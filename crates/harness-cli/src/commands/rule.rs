use clap::Subcommand;
use harness_core::config::HarnessConfig;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum RuleCommand {
    /// Load rules for a project
    Load {
        /// Project directory
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Check project for violations
    Check {
        /// Project directory
        #[arg(default_value = ".")]
        project: PathBuf,
        /// Automatically apply fix_pattern replacements for violations that have one
        #[arg(long)]
        auto_fix: bool,
    },
}

pub(crate) fn configured_rule_engine(config: &HarnessConfig) -> harness_rules::engine::RuleEngine {
    let mut engine = harness_rules::engine::RuleEngine::new();
    engine.configure_sources(
        config.rules.discovery_paths.clone(),
        config.rules.builtin_path.clone(),
        config.rules.requirements_path.clone(),
    );
    engine
}

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
