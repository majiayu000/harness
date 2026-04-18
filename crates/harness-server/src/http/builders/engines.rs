use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

use crate::server::HarnessServer;

/// Outputs of the engine initialization phase.
pub(crate) struct EnginesBundle {
    pub rules: Arc<RwLock<harness_rules::engine::RuleEngine>>,
    pub events: Arc<harness_observe::event_store::EventStore>,
    pub gc_agent: Arc<harness_gc::gc_agent::GcAgent>,
    pub skills: Arc<RwLock<harness_skills::store::SkillStore>>,
}

/// Initialize rule engine, event store, GC agent, and skill store.
///
/// The event store purge background task is spawned here when log retention is
/// configured (>0 days).
pub(crate) async fn build_engines(
    server: &Arc<HarnessServer>,
    data_dir: &Path,
    project_root: &Path,
) -> anyhow::Result<EnginesBundle> {
    use anyhow::Context as _;

    // ── Rule engine ──────────────────────────────────────────────────────────
    let mut rule_engine = harness_rules::engine::RuleEngine::new();
    rule_engine.configure_sources(
        server.config.rules.discovery_paths.clone(),
        server.config.rules.builtin_path.clone(),
        server.config.rules.requirements_path.clone(),
    );
    if let Err(e) = rule_engine.load_builtin() {
        tracing::warn!("failed to load builtin rules: {e}");
    }
    match rule_engine.auto_register_builtin_guards(data_dir) {
        Ok(registered) => {
            tracing::debug!(
                registered_guard_count = registered,
                total_guard_count = rule_engine.guards().len(),
                "rules: builtin guard auto-registration completed"
            );
        }
        Err(e) => {
            tracing::warn!("failed to auto-register builtin guards: {e}");
        }
    }
    match rule_engine.auto_register_project_guards(&project_root.join(".harness/guards")) {
        Ok(registered) => {
            tracing::debug!(
                registered_guard_count = registered,
                total_guard_count = rule_engine.guards().len(),
                "rules: project guard auto-registration completed"
            );
        }
        Err(e) => {
            tracing::warn!("failed to auto-register project guards: {e}");
        }
    }
    // Load guards from each named startup project so non-default projects are
    // not silently unprotected.
    for project in &server.startup_projects {
        match rule_engine.auto_register_project_guards(&project.root.join(".harness/guards")) {
            Ok(registered) => {
                tracing::debug!(
                    project = %project.name,
                    registered_guard_count = registered,
                    total_guard_count = rule_engine.guards().len(),
                    "rules: startup project guard auto-registration completed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    project = %project.name,
                    "failed to auto-register startup project guards: {e}"
                );
            }
        }
    }
    rule_engine
        .load_exec_policy_files(&server.config.rules.exec_policy_paths)
        .context("failed to load rules.exec_policy_paths")?;
    rule_engine
        .load_configured_requirements()
        .context("failed to load configured rules.requirements_path")?;

    // ── Event store ──────────────────────────────────────────────────────────
    let events = Arc::new(
        harness_observe::event_store::EventStore::with_policies_and_otel(
            data_dir,
            server.config.observe.session_renewal_secs,
            server.config.observe.log_retention_days,
            &server.config.otel,
        )
        .await?,
    );

    let retention = server.config.observe.log_retention_days;
    if retention > 0 {
        let purge_events = Arc::clone(&events);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
                if let Err(e) = purge_events.purge_old_events(retention).await {
                    tracing::warn!("event store: periodic purge failed: {e}");
                }
            }
        });
    }

    // ── GC watermark migration ────────────────────────────────────────────────
    // On first boot after upgrading from file-checkpoint to KV-watermark, seed
    // the KV watermark from gc-checkpoint.json so the first incremental scan
    // doesn't regress to a full O(total-events) scan.
    let project_key = project_root.to_string_lossy().into_owned();
    match events.get_scan_watermark(&project_key, "gc").await {
        Ok(None) => {
            // No KV watermark yet — try the legacy file checkpoint.
            let checkpoint_path = harness_gc::checkpoint::default_checkpoint_path(project_root);
            if let Some(cp) = harness_gc::checkpoint::GcCheckpoint::load(&checkpoint_path) {
                match events
                    .set_scan_watermark(&project_key, "gc", cp.last_scan_at)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            ts = %cp.last_scan_at,
                            "gc: migrated legacy checkpoint to KV watermark"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("gc: failed to seed KV watermark from checkpoint: {e}");
                    }
                }
            }
        }
        Ok(Some(_)) => {} // already seeded — nothing to do
        Err(e) => {
            tracing::warn!("gc: failed to read KV watermark during migration check: {e}");
        }
    }

    // ── GC agent ─────────────────────────────────────────────────────────────
    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::from_str(
            project_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default"),
        ),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(data_dir)?;
    // No file checkpoint: QualityTrigger owns the scan watermark via the KV
    // store (get_scan_watermark / set_scan_watermark).  A second file-based
    // cursor in GcAgent would create two independent cursors that can diverge
    // and silently drop events.
    let gc_agent = Arc::new(harness_gc::gc_agent::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        project_root.to_path_buf(),
    ));

    // ── Skill store ───────────────────────────────────────────────────────────
    let mut skill_store = harness_skills::store::SkillStore::new()
        .with_persist_dir(data_dir.join("skills"))
        .with_discovery(project_root);
    skill_store.load_builtin();
    if let Err(e) = skill_store.discover() {
        tracing::warn!("Failed to reload persisted skills on startup: {}", e);
    }

    Ok(EnginesBundle {
        rules: Arc::new(RwLock::new(rule_engine)),
        events,
        gc_agent,
        skills: Arc::new(RwLock::new(skill_store)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;

    fn make_test_server(data_dir: &Path) -> Arc<HarnessServer> {
        let mut config = HarnessConfig::default();
        config.server.data_dir = data_dir.to_path_buf();
        Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ))
    }

    #[tokio::test]
    async fn default_config_loads_builtins_without_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = make_test_server(dir.path());
        let bundle = build_engines(&server, dir.path(), dir.path())
            .await
            .expect("build_engines should succeed");
        // Builtin guards should be registered even without a discovery path.
        let rules = bundle.rules.read().await;
        assert!(
            !rules.guards().is_empty(),
            "expected at least one builtin guard"
        );
    }

    #[tokio::test]
    async fn missing_discovery_path_does_not_abort() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = HarnessConfig::default();
        config.server.data_dir = dir.path().to_path_buf();
        // Point discovery_paths to a non-existent directory.
        config.rules.discovery_paths = vec![dir.path().join("nonexistent")];
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        // Should not return an error even when discovery path is missing.
        let bundle = build_engines(&server, dir.path(), dir.path()).await;
        assert!(bundle.is_ok(), "build_engines failed: {:?}", bundle.err());
        let bundle = bundle.unwrap();
        let rules = bundle.rules.read().await;
        assert!(
            !rules.guards().is_empty(),
            "expected builtins even without discovery path"
        );
    }
}
