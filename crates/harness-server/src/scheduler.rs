use crate::http::AppState;
use harness_core::{EventFilters, Grade};
use harness_observe::health::generate_health_report;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub struct Scheduler {
    pub gc_interval: Duration,
    pub health_interval: Duration,
}

impl Scheduler {
    pub fn from_grade(grade: Grade) -> Self {
        Self {
            gc_interval: grade.recommended_gc_interval(),
            health_interval: Duration::from_secs(24 * 3600),
        }
    }

    pub fn start(self, state: Arc<AppState>) {
        let gc_state = state.clone();
        let gc_interval = self.gc_interval;
        tokio::spawn(async move {
            loop {
                sleep(gc_interval).await;
                tracing::info!("scheduler: triggering periodic GC run");
                crate::handlers::gc::gc_run(&gc_state, None).await;
            }
        });

        let health_state = state;
        let health_interval = self.health_interval;
        tokio::spawn(async move {
            loop {
                sleep(health_interval).await;
                Self::run_health_tick(&health_state).await;
            }
        });
    }

    async fn run_health_tick(state: &AppState) {
        // Query historical events before persisting the current scan to avoid the
        // just-persisted rule_check events inflating the quality stability score.
        let events = match state.events.query(&EventFilters::default()) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("scheduler: failed to query events: {err}");
                return;
            }
        };
        let project_root = state.project_root.clone();
        let violations = {
            let rules = state.rules.read().await;
            rules.scan(&project_root).await.unwrap_or_default()
        };
        state.events.persist_rule_scan(&project_root, &violations);
        let report = generate_health_report(&events, &violations);
        tracing::info!(
            grade = ?report.quality.grade,
            score = report.quality.score,
            violations = report.violation_summary.len(),
            "scheduler: periodic health report"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::{EventFilters, Grade, HarnessConfig};
    use std::{path::Path, sync::Arc};
    use tokio::sync::RwLock;

    async fn make_test_state(dir: &Path, project_root: &Path) -> anyhow::Result<Arc<AppState>> {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        Ok(Arc::new(AppState {
            server,
            project_root: project_root.to_path_buf(),
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            interceptors: vec![],
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }))
    }

    #[test]
    fn from_grade_d_returns_1h_gc_interval() {
        let s = Scheduler::from_grade(Grade::D);
        assert_eq!(s.gc_interval, Duration::from_secs(3600));
        assert_eq!(s.health_interval, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn from_grade_a_returns_7d_gc_interval() {
        let s = Scheduler::from_grade(Grade::A);
        assert_eq!(s.gc_interval, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(s.health_interval, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn from_grade_b_returns_3d_gc_interval() {
        let s = Scheduler::from_grade(Grade::B);
        assert_eq!(s.gc_interval, Duration::from_secs(3 * 24 * 3600));
    }

    #[test]
    fn from_grade_c_returns_1d_gc_interval() {
        let s = Scheduler::from_grade(Grade::C);
        assert_eq!(s.gc_interval, Duration::from_secs(24 * 3600));
    }

    #[tokio::test]
    async fn health_tick_uses_configured_project_root_in_multi_cwd_context() -> anyhow::Result<()> {
        let data_dir = tempfile::tempdir()?;
        let project_root = tempfile::tempdir()?;
        let state = make_test_state(data_dir.path(), project_root.path()).await?;

        let cwd = std::env::current_dir()?;
        assert_ne!(cwd, project_root.path());

        Scheduler::run_health_tick(&state).await;

        let events = state.events.query(&EventFilters::default())?;
        let scan = events
            .iter()
            .rev()
            .find(|event| event.hook == "rule_scan")
            .ok_or_else(|| anyhow::anyhow!("expected scheduler to persist rule_scan event"))?;
        let expected_root = project_root.path().display().to_string();
        assert_eq!(scan.detail.as_deref(), Some(expected_root.as_str()));

        Ok(())
    }
}
