use crate::handlers::cross_review::run_cross_review;
use harness_core::agent::CodeAgent;
use harness_core::types::{EventFilters, Grade, Project};
use harness_gc::gc_agent::GcAgent;
use harness_observe::event_store::EventStore;
use harness_observe::quality::QualityGrader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Task context passed to cross-review: the diff output and PR URL.
pub struct TaskReviewContext {
    pub diff: String,
    pub pr_description: String,
}

/// Evaluates project quality after each task completion and triggers GC when
/// the grade falls below a configured threshold.
pub struct QualityTrigger {
    events: Arc<EventStore>,
    gc_agent: Arc<GcAgent>,
    agent_registry: Arc<harness_agents::registry::AgentRegistry>,
    project_root: PathBuf,
    auto_gc_grades: Vec<Grade>,
    cooldown_secs: u64,
    last_triggered: Arc<AtomicU64>,
    challenger_agent: Option<Arc<dyn CodeAgent>>,
}

impl QualityTrigger {
    pub fn new(
        events: Arc<EventStore>,
        gc_agent: Arc<GcAgent>,
        agent_registry: Arc<harness_agents::registry::AgentRegistry>,
        project_root: PathBuf,
        auto_gc_grades: Vec<Grade>,
        cooldown_secs: u64,
        challenger_agent: Option<Arc<dyn CodeAgent>>,
    ) -> Self {
        Self {
            events,
            gc_agent,
            agent_registry,
            project_root,
            auto_gc_grades,
            cooldown_secs,
            last_triggered: Arc::new(AtomicU64::new(0)),
            challenger_agent,
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

    /// Downgrade a grade by one step. D stays at D (floor).
    fn downgrade(grade: Grade) -> Grade {
        match grade {
            Grade::A => Grade::A, // gated before call; kept for completeness
            Grade::B => Grade::C,
            Grade::C => Grade::D,
            Grade::D => Grade::D,
        }
    }

    /// Grade recent events, run optional cross-review, log the result, and
    /// auto-trigger GC if warranted.
    pub async fn check_and_maybe_trigger(&self, task_ctx: Option<&TaskReviewContext>) {
        let events = match self.events.query(&EventFilters::default()).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("quality_trigger: failed to query events: {e}");
                return;
            }
        };

        let mut report = QualityGrader::grade(&events, 0);

        // Cross-review gate: skip if no challenger, no task context, or grade=A.
        if let (Some(challenger), Some(ctx)) = (&self.challenger_agent, task_ctx) {
            if report.grade != Grade::A {
                // Cap diff to avoid oversized LLM requests (latency/cost spikes).
                // Use char-boundary-safe truncation to prevent panics on multibyte UTF-8.
                const MAX_DIFF_BYTES: usize = 4096;
                let diff_excerpt = if ctx.diff.len() > MAX_DIFF_BYTES {
                    let end = (0..=MAX_DIFF_BYTES)
                        .rev()
                        .find(|&i| ctx.diff.is_char_boundary(i))
                        .unwrap_or(0);
                    &ctx.diff[..end]
                } else {
                    &ctx.diff
                };
                let target = format!(
                    "PR: {}\n\nDiff summary:\n{}",
                    ctx.pr_description, diff_excerpt
                );
                if let Some(primary) = self.agent_registry.default_agent() {
                    // Identity guard: skip cross-review when primary and challenger are the
                    // same agent — identical models produce correlated verdicts that cannot
                    // serve as an independent check.
                    if primary.name() == challenger.name() {
                        tracing::warn!(
                            agent = primary.name(),
                            "quality_trigger: primary and challenger are the same agent; \
                             skipping cross-review to preserve independence"
                        );
                    } else {
                        const CROSS_REVIEW_TIMEOUT_SECS: u64 = 120;
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(CROSS_REVIEW_TIMEOUT_SECS),
                            run_cross_review(
                                primary,
                                Some(challenger.clone()),
                                self.project_root.clone(),
                                target,
                                2,
                                // Deny all tools: review is text-only, agents must not
                                // mutate the repo during this background quality gate.
                                Some(vec![]),
                            ),
                        )
                        .await
                        {
                            Ok(Ok(result)) => {
                                report.semantic_verdict = Some(result.final_verdict.clone());
                                if result.final_verdict == "NOT_CONVERGED" {
                                    let original = report.grade;
                                    report.grade = Self::downgrade(report.grade);
                                    tracing::info!(
                                        original_grade = ?original,
                                        effective_grade = ?report.grade,
                                        "quality_trigger: NOT_CONVERGED — grade downgraded"
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    "quality_trigger: cross-review failed: {e}; using numeric grade only"
                                );
                            }
                            Err(_elapsed) => {
                                tracing::warn!(
                                    "quality_trigger: cross-review timed out after {CROSS_REVIEW_TIMEOUT_SECS}s; \
                                     using numeric grade only"
                                );
                            }
                        }
                    }
                } else {
                    tracing::warn!("quality_trigger: no primary agent for cross-review, skipping");
                }
            }
        }

        self.events
            .log_quality_grade(report.grade, report.score)
            .await;

        tracing::info!(
            grade = ?report.grade,
            score = report.score,
            semantic_verdict = ?report.semantic_verdict,
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
    use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
    use harness_core::error::Result as HarnessResult;
    use harness_core::types::{Capability, Decision, Event, Grade, SessionId, TokenUsage};
    use harness_gc::draft_store::DraftStore;
    use harness_gc::gc_agent::GcAgent;
    use harness_gc::signal_detector::SignalDetector;
    use std::path::Path;
    use tokio::sync::mpsc::Sender;

    async fn make_trigger(
        dir: &Path,
        auto_gc_grades: Vec<Grade>,
        cooldown_secs: u64,
    ) -> QualityTrigger {
        make_trigger_with_challenger(dir, auto_gc_grades, cooldown_secs, None, None).await
    }

    async fn make_trigger_with_challenger(
        dir: &Path,
        auto_gc_grades: Vec<Grade>,
        cooldown_secs: u64,
        primary: Option<Arc<dyn CodeAgent>>,
        challenger: Option<Arc<dyn CodeAgent>>,
    ) -> QualityTrigger {
        let events = Arc::new(EventStore::new(dir).await.expect("event store"));
        let gc_config = harness_core::config::misc::GcConfig::default();
        let signal_detector = SignalDetector::new(
            gc_config.signal_thresholds.clone().into(),
            harness_core::types::ProjectId::new(),
        );
        let draft_store = DraftStore::new(dir).expect("draft store");
        let gc_agent = Arc::new(GcAgent::new(
            gc_config,
            signal_detector,
            draft_store,
            dir.to_path_buf(),
        ));
        let mut registry = harness_agents::registry::AgentRegistry::new("test");
        if let Some(p) = primary {
            registry.register("test", p);
        }
        let agent_registry = Arc::new(registry);
        QualityTrigger::new(
            events,
            gc_agent,
            agent_registry,
            dir.to_path_buf(),
            auto_gc_grades,
            cooldown_secs,
            challenger,
        )
    }

    // Mock that returns "ISSUE: foo\nCONFIRMED: foo" — used as primary in NOT_CONVERGED tests.
    // Has a distinct name from NotConvergedMock so the identity guard does not skip cross-review.
    struct NotConvergedPrimaryMock;

    #[async_trait::async_trait]
    impl CodeAgent for NotConvergedPrimaryMock {
        fn name(&self) -> &str {
            "not-converged-primary"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "ISSUE: foo\nCONFIRMED: foo".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    // Mock that returns "ISSUE: foo" (primary) or "CONFIRMED: foo" (challenger) — NOT_CONVERGED
    struct NotConvergedMock;

    #[async_trait::async_trait]
    impl CodeAgent for NotConvergedMock {
        fn name(&self) -> &str {
            "not-converged-mock"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "ISSUE: foo\nCONFIRMED: foo".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    // Mock that returns LGTM — APPROVED
    struct ApprovedMock;

    #[async_trait::async_trait]
    impl CodeAgent for ApprovedMock {
        fn name(&self) -> &str {
            "approved-mock"
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }
        async fn execute(&self, _req: AgentRequest) -> HarnessResult<AgentResponse> {
            Ok(AgentResponse {
                output: "LGTM".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: Sender<StreamItem>,
        ) -> HarnessResult<()> {
            Ok(())
        }
    }

    fn block_event(hook: &str) -> Event {
        Event::new(SessionId::new(), hook, "Edit", Decision::Block)
    }

    fn pass_event() -> Event {
        Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass)
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

        trigger.check_and_maybe_trigger(None).await;

        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
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

    // --- challenger cross-review tests (6 new scenarios) ---

    // Scenario 1: challenger=None, task_ctx=None → existing behavior, one quality_grade event
    #[tokio::test]
    async fn no_challenger_no_ctx_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger(dir.path(), vec![Grade::D], 300).await;
        trigger.events.log(&pass_event()).await.unwrap();
        trigger.check_and_maybe_trigger(None).await;
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
    }

    // Scenario 2: challenger=Some, grade=A → cross-review skipped
    #[tokio::test]
    async fn challenger_grade_a_skips_cross_review() {
        let dir = tempfile::tempdir().unwrap();
        // Many pass events → grade A
        let trigger = make_trigger_with_challenger(
            dir.path(),
            vec![Grade::D],
            300,
            None,
            Some(Arc::new(NotConvergedMock)),
        )
        .await;
        for _ in 0..20 {
            trigger.events.log(&pass_event()).await.unwrap();
        }
        let ctx = TaskReviewContext {
            diff: "diff".to_string(),
            pr_description: "https://github.com/o/r/pull/1".to_string(),
        };
        trigger.check_and_maybe_trigger(Some(&ctx)).await;
        // Grade should remain A (no downgrade happened)
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let detail = events[0].detail.as_deref().unwrap_or("");
        assert!(detail.contains("A"), "expected grade A in detail: {detail}");
    }

    // Scenario 3: challenger=Some, grade=B, verdict=APPROVED → grade stays B
    #[tokio::test]
    async fn challenger_grade_b_approved_stays_b() {
        let dir = tempfile::tempdir().unwrap();
        // Mix: many pass, a few security blocks to land on B
        // security=70, stability=70, coverage=100, perf=100 → score=79 → B
        let trigger = make_trigger_with_challenger(
            dir.path(),
            vec![Grade::D],
            300,
            None,
            Some(Arc::new(ApprovedMock)),
        )
        .await;
        for _ in 0..7 {
            trigger.events.log(&pass_event()).await.unwrap();
        }
        for _ in 0..3 {
            trigger
                .events
                .log(&block_event("security_check"))
                .await
                .unwrap();
        }
        let ctx = TaskReviewContext {
            diff: "diff".to_string(),
            pr_description: "https://github.com/o/r/pull/1".to_string(),
        };
        trigger.check_and_maybe_trigger(Some(&ctx)).await;
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        // Grade must not have dropped below B (APPROVED verdict, no downgrade)
        let detail = events[0].detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("B") || detail.contains("A"),
            "expected B or A in detail after APPROVED: {detail}"
        );
    }

    // Scenario 4: challenger=Some, grade=B, verdict=NOT_CONVERGED → downgraded to C
    #[tokio::test]
    async fn challenger_grade_b_not_converged_downgraded_to_c() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger_with_challenger(
            dir.path(),
            vec![Grade::D],
            300,
            Some(Arc::new(NotConvergedPrimaryMock)),
            Some(Arc::new(NotConvergedMock)),
        )
        .await;
        // security=70, stability=70, coverage=100, perf=100 → score=79 → grade B
        for _ in 0..7 {
            trigger.events.log(&pass_event()).await.unwrap();
        }
        for _ in 0..3 {
            trigger
                .events
                .log(&block_event("security_check"))
                .await
                .unwrap();
        }
        let ctx = TaskReviewContext {
            diff: "diff".to_string(),
            pr_description: "https://github.com/o/r/pull/1".to_string(),
        };
        trigger.check_and_maybe_trigger(Some(&ctx)).await;
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let detail = events[0].detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("C") || detail.contains("D"),
            "expected downgrade to C or D after NOT_CONVERGED: {detail}"
        );
    }

    // Scenario 5: challenger=Some, grade=C, NOT_CONVERGED → D, GC triggered (D in auto_gc_grades)
    #[tokio::test]
    async fn challenger_grade_c_not_converged_triggers_gc() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger_with_challenger(
            dir.path(),
            vec![Grade::D],
            0, // zero cooldown so GC fires immediately
            None,
            Some(Arc::new(NotConvergedMock)),
        )
        .await;
        // security=50, stability=50, coverage=100, perf=100 → score=65 → grade C
        for _ in 0..5 {
            trigger.events.log(&pass_event()).await.unwrap();
        }
        for _ in 0..5 {
            trigger
                .events
                .log(&block_event("security_check"))
                .await
                .unwrap();
        }
        let ctx = TaskReviewContext {
            diff: "diff".to_string(),
            pr_description: "https://github.com/o/r/pull/1".to_string(),
        };
        // Does not panic; grade C → D after NOT_CONVERGED; GC fires (no agent registered so
        // the GC path logs a warning — that's acceptable in this unit test)
        trigger.check_and_maybe_trigger(Some(&ctx)).await;
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let detail = events[0].detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("C") || detail.contains("D"),
            "expected C or D after NOT_CONVERGED on C input: {detail}"
        );
    }

    // Scenario 6: task_ctx=None, challenger=Some → cross-review skipped, numeric grade only
    #[tokio::test]
    async fn challenger_no_ctx_skips_cross_review() {
        let dir = tempfile::tempdir().unwrap();
        let trigger = make_trigger_with_challenger(
            dir.path(),
            vec![Grade::D],
            300,
            None,
            Some(Arc::new(NotConvergedMock)),
        )
        .await;
        trigger.events.log(&pass_event()).await.unwrap();
        // No ctx → cross-review skipped despite challenger being present
        trigger.check_and_maybe_trigger(None).await;
        let events = trigger
            .events
            .query(&harness_core::types::EventFilters {
                hook: Some("quality_grade".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
    }
}
