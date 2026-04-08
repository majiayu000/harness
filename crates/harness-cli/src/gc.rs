use harness_core::{
    config::misc::GcConfig,
    types::{Draft, DraftId, DraftStatus, EventFilters, Project, ProjectId},
};
use harness_gc::draft_store::DraftStore;
use harness_gc::gc_agent::GcAgent;
use harness_gc::signal_detector::SignalDetector;
use harness_gc::signal_detector::SignalThresholds as GcThresholds;
use harness_observe::event_store::EventStore;
use std::sync::Arc;

use crate::commands::GcCommand;

pub async fn run_gc(
    cmd: GcCommand,
    config: &harness_core::config::HarnessConfig,
) -> anyhow::Result<()> {
    let data_dir = &config.server.data_dir;

    match cmd {
        GcCommand::Run { project } => {
            let project_root = match project {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let project_id = project_id_from_root(&project_root);
            let project = Project {
                id: project_id.clone(),
                root: project_root.clone(),
                languages: Vec::new(),
                name: project_root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
            };

            let event_store = EventStore::with_policies_and_otel(
                data_dir,
                config.observe.session_renewal_secs,
                config.observe.log_retention_days,
                &config.otel,
            )
            .await?;
            let events = event_store.query(&EventFilters::default()).await?;
            let external_signals = event_store.query_external_signals(None)?;
            event_store.shutdown().await;

            let thresholds = map_thresholds(&config.gc.signal_thresholds);
            let signal_detector = SignalDetector::new(thresholds, project_id);
            let gc_config = map_gc_config(&config.gc);
            let draft_store = DraftStore::new(data_dir)?;
            let gc_agent = GcAgent::new(
                gc_config,
                signal_detector,
                draft_store,
                project.root.clone(),
            );

            let agent_registry = build_agent_registry(config);
            let agent = agent_registry.default_agent().ok_or_else(|| {
                anyhow::anyhow!(
                    "no default agent registered; available agents: {}",
                    agent_registry.list().join(", ")
                )
            })?;

            let report = gc_agent
                .run_with_external(&project, &events, &[], &external_signals, agent.as_ref())
                .await?;
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

        GcCommand::Drafts { project } => {
            let draft_store = DraftStore::new(data_dir)?;
            let drafts = draft_store.list()?;
            let project_filter = project.as_ref().map(|p| project_id_from_root(p));
            let pending: Vec<_> = drafts
                .iter()
                .filter(|d| {
                    matches!(d.status, DraftStatus::Pending)
                        && project_filter
                            .as_ref()
                            .map(|pid| d.signal.project_id == *pid)
                            .unwrap_or(true)
                })
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
            let project_root = config.server.project_root.clone();
            let gc_agent = make_agent_for_draft_ops(data_dir, &project_root)?;
            gc_agent.adopt(&id)?;
            println!("Adopted draft: {draft_id}");
        }

        GcCommand::Reject { draft_id, reason } => {
            let id = DraftId::from_str(&draft_id);
            let project_root = config.server.project_root.clone();
            let gc_agent = make_agent_for_draft_ops(data_dir, &project_root)?;
            gc_agent.reject(&id, reason.as_deref())?;
            println!("Rejected draft: {draft_id}");
        }
    }

    Ok(())
}

fn make_agent_for_draft_ops(
    data_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> anyhow::Result<GcAgent> {
    let draft_store = DraftStore::new(data_dir)?;
    let project_id = project_id_from_root(project_root);
    Ok(GcAgent::new(
        GcConfig::default(),
        SignalDetector::new(GcThresholds::default(), project_id),
        draft_store,
        project_root.to_path_buf(),
    ))
}

fn build_agent_registry(
    config: &harness_core::config::HarnessConfig,
) -> harness_agents::registry::AgentRegistry {
    let mut registry = harness_agents::registry::AgentRegistry::new(&config.agents.default_agent);
    registry.set_complexity_preferences(config.agents.complexity_preferred_agents.clone());
    registry.register(
        "claude",
        Arc::new(
            harness_agents::claude::ClaudeCodeAgent::new(
                config.agents.claude.cli_path.clone(),
                config.agents.claude.default_model.clone(),
                config.agents.sandbox_mode,
            )
            .with_no_session_persistence_probe()
            .with_stream_timeout(config.agents.stream_timeout_secs),
        ),
    );
    registry.register(
        "codex",
        Arc::new(
            harness_agents::codex::CodexAgent::from_config(
                config.agents.codex.clone(),
                config.agents.sandbox_mode,
            )
            .with_stream_timeout(config.agents.stream_timeout_secs),
        ),
    );
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        registry.register(
            "anthropic-api",
            Arc::new(
                harness_agents::anthropic_api::AnthropicApiAgent::from_config(
                    api_key,
                    &config.agents.anthropic_api,
                ),
            ),
        );
    }
    registry
}

fn project_id_from_root(project_root: &std::path::Path) -> ProjectId {
    ProjectId::from_str(
        project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default"),
    )
}

fn count_by_status(drafts: &[Draft], status: DraftStatus) -> usize {
    drafts.iter().filter(|d| d.status == status).count()
}

fn map_thresholds(t: &harness_core::config::misc::SignalThresholdsConfig) -> GcThresholds {
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

fn map_gc_config(c: &harness_core::config::misc::GcConfig) -> GcConfig {
    c.clone()
}
