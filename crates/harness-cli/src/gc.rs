use harness_core::{Draft, DraftId, DraftStatus, EventFilters, Project, ProjectId};
use harness_gc::gc_agent::GcConfig as GcAgentConfig;
use harness_gc::signal_detector::SignalThresholds as GcThresholds;
use harness_gc::{DraftStore, GcAgent, SignalDetector};
use harness_observe::EventStore;

use crate::commands::GcCommand;

pub async fn run_gc(cmd: GcCommand, config: &harness_core::HarnessConfig) -> anyhow::Result<()> {
    let data_dir = &config.server.data_dir;

    match cmd {
        GcCommand::Run { project } => {
            let project_root = match project {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let project = Project::from_path(project_root);

            let event_store = EventStore::with_policies_and_otel(
                data_dir,
                config.observe.session_renewal_secs,
                config.observe.log_retention_days,
                &config.otel,
            )
            .await?;
            let events = event_store.query(&EventFilters::default())?;
            event_store.shutdown().await;

            let thresholds = map_thresholds(&config.gc.signal_thresholds);
            let signal_detector = SignalDetector::new(thresholds, project.id.clone());
            let gc_config = map_gc_config(&config.gc);
            let draft_store = DraftStore::new(data_dir)?;
            let gc_agent = GcAgent::new(gc_config, signal_detector, draft_store);

            let claude = harness_agents::claude::ClaudeCodeAgent::new(
                config.agents.claude.cli_path.clone(),
                config.agents.claude.default_model.clone(),
                config.agents.sandbox_mode,
            );

            let report = gc_agent.run(&project, &events, &[], &claude).await?;
            println!("Signals detected: {}", report.signals.len());
            println!("Drafts generated: {}", report.drafts_generated);
            for e in &report.errors {
                eprintln!("Error: {e}");
            }
        }

        GcCommand::Status => {
            let draft_store = DraftStore::new(data_dir)?;
            let drafts = draft_store.list()?;
            let pending = count_by_status(&drafts, DraftStatus::Pending);
            let adopted = count_by_status(&drafts, DraftStatus::Adopted);
            let rejected = count_by_status(&drafts, DraftStatus::Rejected);
            println!("GC status: {} total drafts", drafts.len());
            println!("  pending:  {pending}");
            println!("  adopted:  {adopted}");
            println!("  rejected: {rejected}");
        }

        GcCommand::Drafts { .. } => {
            let draft_store = DraftStore::new(data_dir)?;
            let drafts = draft_store.list()?;
            let pending: Vec<_> = drafts
                .iter()
                .filter(|d| matches!(d.status, DraftStatus::Pending))
                .collect();
            if pending.is_empty() {
                println!("No pending drafts");
            } else {
                for draft in &pending {
                    println!(
                        "{} [{:?}] {}",
                        draft.id, draft.signal.signal_type, draft.rationale
                    );
                }
            }
        }

        GcCommand::Adopt { draft_id } => {
            let id = DraftId::from_str(&draft_id);
            let gc_agent = make_agent_for_draft_ops(data_dir)?;
            gc_agent.adopt(&id)?;
            println!("Adopted draft: {draft_id}");
        }

        GcCommand::Reject { draft_id, reason } => {
            let id = DraftId::from_str(&draft_id);
            let gc_agent = make_agent_for_draft_ops(data_dir)?;
            gc_agent.reject(&id, reason.as_deref())?;
            println!("Rejected draft: {draft_id}");
        }
    }

    Ok(())
}

fn make_agent_for_draft_ops(data_dir: &std::path::Path) -> anyhow::Result<GcAgent> {
    let draft_store = DraftStore::new(data_dir)?;
    Ok(GcAgent::new(
        GcAgentConfig::default(),
        SignalDetector::new(GcThresholds::default(), ProjectId::new()),
        draft_store,
    ))
}

fn count_by_status(drafts: &[Draft], status: DraftStatus) -> usize {
    drafts.iter().filter(|d| d.status == status).count()
}

fn map_thresholds(t: &harness_core::SignalThresholds) -> GcThresholds {
    GcThresholds {
        repeated_warn_min: t.repeated_warn_min,
        chronic_block_min: t.chronic_block_min,
        hot_file_edits_min: t.hot_file_edits_min,
        slow_op_threshold_ms: t.slow_op_threshold_ms,
        slow_op_count_min: t.slow_op_count_min,
        escalation_ratio: t.escalation_ratio,
        violation_min: t.violation_min,
    }
}

fn map_gc_config(c: &harness_core::GcConfig) -> GcAgentConfig {
    GcAgentConfig {
        max_drafts_per_run: c.max_drafts_per_run,
        budget_per_signal_usd: c.budget_per_signal_usd,
        total_budget_usd: c.total_budget_usd,
    }
}
