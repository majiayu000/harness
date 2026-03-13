use harness_core::{EventFilters, Grade, Project};
use harness_gc::GcAgent;
use harness_observe::{EventStore, QualityGrader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Evaluates project quality after each task completion and triggers GC when
/// the grade falls below a configured threshold.
pub struct QualityTrigger {
    events: Arc<EventStore>,
    gc_agent: Arc<GcAgent>,
    agent_registry: Arc<harness_agents::AgentRegistry>,
    project_root: PathBuf,
    auto_gc_grades: Vec<Grade>,
    cooldown_secs: u64,
    last_triggered: Arc<AtomicU64>,
}

impl QualityTrigger {
    pub fn new(
        events: Arc<EventStore>,
        gc_agent: Arc<GcAgent>,
        agent_registry: Arc<harness_agents::AgentRegistry>,
        project_root: PathBuf,
        auto_gc_grades: Vec<Grade>,
        cooldown_secs: u64,
    ) -> Self {
        Self {
            events,
            gc_agent,
            agent_registry,
            project_root,
            auto_gc_grades,
            cooldown_secs,
            last_triggered: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns true if the given grade should trigger an auto-GC run.
    pub fn grade_triggers_gc(&self, grade: Grade) -> bool {
        self.auto_gc_grades.contains(&grade)
    }

    /// Returns true if the cooldown period has elapsed since the last trigger.
    fn cooldown_elapsed(&self) -> bool {
        let last = self.last_triggered.load(Ordering::Relaxed);
        unix_now().saturating_sub(last) >= self.cooldown_secs
    }

    /// Grade recent events, log the result, and auto-trigger GC if warranted.
    pub async fn check_and_maybe_trigger(&self) {
        let events = match self.events.query(&EventFilters::default()).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("quality_trigger: failed to query events: {e}");
                return;
            }
        };

        let report = QualityGrader::grade(&events, 0);
        self.events
            .log_quality_grade(report.grade, report.score)
            .await;

        tracing::info!(
            grade = ?report.grade,
            score = report.score,
            "quality_trigger: post-task quality check"
        );

        if !self.grade_triggers_gc(report.grade) {
            return;
        }

        if !self.cooldown_elapsed() {
            tracing::debug!(
                grade = ?report.grade,
                cooldown_secs = self.cooldown_secs,
                "quality_trigger: grade triggers GC but cooldown not elapsed, skipping"
            );
            return;
        }

        tracing::info!(
            grade = ?report.grade,
            "quality_trigger: grade triggers auto-GC run"
        );
        self.last_triggered.store(unix_now(), Ordering::Relaxed);

        let Some(agent) = self.agent_registry.default_agent() else {
            tracing::warn!("quality_trigger: no agent registered, skipping auto-GC");
            return;
        };
        let project = Project::from_path(self.project_root.clone());
        if let Err(e) = self
            .gc_agent
            .run(&project, &events, &[], agent.as_ref())
            .await
        {
            tracing::warn!("quality_trigger: gc_agent.run failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, Grade, SessionId};
    use harness_gc::{DraftStore, GcAgent, SignalDetector};
    use std::path::Path;

    async fn make_trigger(
        dir: &Path,
        auto_gc_grades: Vec<Grade>,
        cooldown_secs: u64,
    ) -> QualityTrigger {
        let events = Arc::new(EventStore::new(dir).await.expect("event store"));
        let signal_detector = SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = DraftStore::new(dir).expect("draft store");
        let gc_agent = Arc::new(GcAgent::new(
            harness_core::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let agent_registry = Arc::new(harness_agents::AgentRegistry::new("test"));
        QualityTrigger::new(
            events,
            gc_agent,
            agent_registry,
            dir.to_path_buf(),
            auto_gc_grades,
            cooldown_secs,
        )
    }

    // --- grade_triggers_gc mapping tests ---

    #[tokio::test]
    async fn grade_d_triggers_gc_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        assert!(trigger.grade_triggers_gc(Grade::D));
    }

    #[tokio::test]
    async fn grade_a_does_not_trigger_gc_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        assert!(!trigger.grade_triggers_gc(Grade::A));
    }

    #[tokio::test]
    async fn grade_b_does_not_trigger_gc_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        assert!(!trigger.grade_triggers_gc(Grade::B));
    }

    #[tokio::test]
    async fn grade_c_does_not_trigger_gc_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        assert!(!trigger.grade_triggers_gc(Grade::C));
    }

    #[tokio::test]
    async fn configuring_c_and_d_both_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::C, Grade::D], 300).await;
        assert!(trigger.grade_triggers_gc(Grade::C));
        assert!(trigger.grade_triggers_gc(Grade::D));
        assert!(!trigger.grade_triggers_gc(Grade::A));
        assert!(!trigger.grade_triggers_gc(Grade::B));
    }

    #[tokio::test]
    async fn empty_auto_gc_grades_never_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![], 300).await;
        assert!(!trigger.grade_triggers_gc(Grade::A));
        assert!(!trigger.grade_triggers_gc(Grade::B));
        assert!(!trigger.grade_triggers_gc(Grade::C));
        assert!(!trigger.grade_triggers_gc(Grade::D));
    }

    // --- cooldown tests ---

    #[tokio::test]
    async fn cooldown_elapsed_when_never_triggered() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        assert!(trigger.cooldown_elapsed());
    }

    #[tokio::test]
    async fn cooldown_not_elapsed_immediately_after_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        trigger.last_triggered.store(unix_now(), Ordering::Relaxed);
        assert!(!trigger.cooldown_elapsed());
    }

    #[tokio::test]
    async fn zero_cooldown_always_elapsed() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 0).await;
        trigger.last_triggered.store(unix_now(), Ordering::Relaxed);
        assert!(trigger.cooldown_elapsed());
    }

    // --- log_quality_grade integration test ---

    #[tokio::test]
    async fn check_logs_quality_grade_event() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;

        // Seed a passing event so grade comes back as A (score ~ 100)
        trigger
            .events
            .log(&Event::new(
                SessionId::new(),
                "pre_tool_use",
                "Edit",
                Decision::Pass,
            ))
            .await
            .unwrap();

        trigger.check_and_maybe_trigger().await;

        let events = trigger
            .events
            .query(&harness_core::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "expected exactly one quality_grade event");
        assert!(events[0]
            .detail
            .as_deref()
            .unwrap_or("")
            .starts_with("grade="));
    }
}
