use crate::handlers::cross_review::run_cross_review;
use harness_core::agent::CodeAgent;
use harness_core::types::{Capability, EventFilters, Grade, Project};
use harness_gc::gc_agent::GcAgent;
use harness_observe::event_store::EventStore;
use harness_observe::quality::QualityGrader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) fn unix_now() -> u64 {
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
    pub(crate) events: Arc<EventStore>,
    gc_agent: Arc<GcAgent>,
    agent_registry: Arc<harness_agents::registry::AgentRegistry>,
    project_root: PathBuf,
    auto_gc_grades: Vec<Grade>,
    cooldown_secs: u64,
    pub(crate) last_triggered: Arc<AtomicU64>,
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
    pub(crate) fn cooldown_elapsed(&self) -> bool {
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
                    } else if primary
                        .capabilities()
                        .iter()
                        .any(|c| matches!(c, Capability::Write | Capability::Execute))
                    {
                        // Guard the primary as well: agents that advertise Write or
                        // Execute capabilities (e.g. CodexAgent) ignore allowed_tools
                        // and run with their configured sandbox, potentially mutating
                        // the workspace during this background quality gate.
                        tracing::warn!(
                            agent = primary.name(),
                            "quality_trigger: primary agent has Write/Execute capabilities \
                             and does not honour allowed_tools; skipping cross-review \
                             to prevent workspace mutation"
                        );
                    } else {
                        // Only allow the challenger when its capabilities are
                        // read-only.  Agents that advertise Write or Execute
                        // capabilities (e.g. CodexAgent) do not honour
                        // allowed_tools and will run with their configured
                        // sandbox, potentially mutating the workspace.
                        let review_challenger = if challenger
                            .capabilities()
                            .iter()
                            .any(|c| matches!(c, Capability::Write | Capability::Execute))
                        {
                            tracing::warn!(
                                agent = challenger.name(),
                                "quality_trigger: challenger has Write/Execute capabilities \
                                 and does not honour allowed_tools; skipping as \
                                 cross-review challenger to prevent workspace mutation"
                            );
                            None
                        } else {
                            Some(challenger.clone())
                        };
                        const CROSS_REVIEW_TIMEOUT_SECS: u64 = 120;
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(CROSS_REVIEW_TIMEOUT_SECS),
                            run_cross_review(
                                primary,
                                review_challenger,
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
