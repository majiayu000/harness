use harness_core::{types::Decision, types::Event, types::Grade};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub score: f64,
    pub grade: Grade,
    pub dimensions: QualityDimensions,
    pub recommended_gc_interval: std::time::Duration,
    /// Semantic verdict from challenger-agent cross-review.
    /// "APPROVED" | "NOT_CONVERGED" | None (cross-review not run)
    pub semantic_verdict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityDimensions {
    pub security: f64,
    pub stability: f64,
    pub coverage: f64,
    pub performance: f64,
}

pub struct QualityGrader;

const DERIVED_QUALITY_GRADE_HOOK: &str = "quality_grade";

fn gradeable_events(events: &[Event]) -> impl Iterator<Item = &Event> {
    events
        .iter()
        .filter(|event| event.hook != DERIVED_QUALITY_GRADE_HOOK)
}

impl QualityGrader {
    /// Grade an observation window, or `None` when the window holds no
    /// independent evidence at all.
    ///
    /// Every dimension below is a ratio over observed events. With zero events
    /// and zero violations each ratio degenerates to its perfect value and the
    /// window scores 100/A — a fabricated verdict indistinguishable from a
    /// genuinely clean window, which then drives GC cadence and dashboards. No
    /// data is not good news, so it gets no grade. Previously emitted
    /// `quality_grade` events are derived output and are excluded from both the
    /// evidence check and every ratio so a fabricated grade cannot perpetuate
    /// or dilute a later verdict.
    pub fn grade(events: &[Event], violation_count: usize) -> Option<QualityReport> {
        let gradeable_event_count = gradeable_events(events).count();
        if gradeable_event_count == 0 && violation_count == 0 {
            return None;
        }
        let total = gradeable_event_count.max(1) as f64;

        // Security: ratio of security-related blocks to total events
        let security_issues = gradeable_events(events)
            .filter(|e| matches!(e.decision, Decision::Block) && e.hook.contains("security"))
            .count() as f64;
        let security = (1.0 - security_issues / total) * 100.0;

        // Stability: ratio of non-failed events
        let failures = gradeable_events(events)
            .filter(|e| matches!(e.decision, Decision::Block | Decision::Escalate))
            .count() as f64;
        let stability = (1.0 - failures / total) * 100.0;

        // Coverage: inverse of violation density
        let coverage = if violation_count == 0 {
            100.0
        } else {
            (1.0 - (violation_count as f64 / 100.0).min(1.0)) * 100.0
        };

        // Performance: ratio of fast operations
        let slow_ops = gradeable_events(events)
            .filter(|e| e.duration_ms.map(|d| d > 5000).unwrap_or(false))
            .count() as f64;
        let performance = (1.0 - slow_ops / total) * 100.0;

        // Weighted score: security × 0.4 + stability × 0.3 + coverage × 0.2 + perf × 0.1
        let score = security * 0.4 + stability * 0.3 + coverage * 0.2 + performance * 0.1;
        let grade = Grade::from_score(score);

        Some(QualityReport {
            score,
            grade,
            dimensions: QualityDimensions {
                security,
                stability,
                coverage,
                performance,
            },
            recommended_gc_interval: grade.recommended_gc_interval(),
            semantic_verdict: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{types::Decision, types::Event, types::Grade, types::SessionId};

    fn pass_event() -> Event {
        Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass)
    }

    fn block_event(hook: &str) -> Event {
        Event::new(SessionId::new(), hook, "Edit", Decision::Block)
    }

    fn quality_grade_event() -> Event {
        Event::new(
            SessionId::new(),
            DERIVED_QUALITY_GRADE_HOOK,
            "QualityGrader",
            Decision::Pass,
        )
    }

    #[test]
    fn grade_perfect_events_no_violations() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 0).expect("a non-empty window is gradable");
        assert_eq!(report.grade, Grade::A);
        assert!(report.score >= 90.0);
    }

    #[test]
    fn grade_degrades_with_violations() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 50).expect("a non-empty window is gradable");
        assert!(report.score < 100.0);
        assert!(report.dimensions.coverage < 100.0);
    }

    #[test]
    fn grade_degrades_with_many_blocks() {
        let events: Vec<Event> = (0..10).map(|_| block_event("security_check")).collect();
        let report = QualityGrader::grade(&events, 0).expect("a non-empty window is gradable");
        assert!(report.dimensions.stability < 100.0);
    }

    #[test]
    fn empty_window_has_no_verdict() {
        assert!(QualityGrader::grade(&[], 0).is_none());
    }

    #[test]
    fn violations_without_events_are_still_graded() {
        let report =
            QualityGrader::grade(&[], 5).expect("violations are evidence even without events");
        assert!(report.dimensions.coverage < 100.0);
    }

    #[test]
    fn a_clean_non_empty_window_still_scores_perfectly() {
        let events: Vec<Event> = (0..3).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 0).expect("a non-empty window is gradable");
        assert_eq!(report.grade, Grade::A);
        assert_eq!(report.score, 100.0);
    }

    #[test]
    fn derived_grade_events_do_not_create_a_verdict() {
        assert!(QualityGrader::grade(&[quality_grade_event()], 0).is_none());
    }

    #[test]
    fn derived_grade_events_do_not_dilute_independent_evidence() {
        let independent_event = block_event("security_check");
        let baseline = QualityGrader::grade(std::slice::from_ref(&independent_event), 0)
            .expect("an independent event is gradable");
        let with_derived_output =
            QualityGrader::grade(&[independent_event, quality_grade_event()], 0)
                .expect("derived output must not hide independent evidence");

        assert_eq!(with_derived_output.grade, baseline.grade);
        assert_eq!(with_derived_output.score, baseline.score);
        assert_eq!(
            with_derived_output.dimensions.security,
            baseline.dimensions.security
        );
        assert_eq!(
            with_derived_output.dimensions.stability,
            baseline.dimensions.stability
        );
    }

    #[test]
    fn violations_remain_gradeable_with_only_derived_events() {
        let report = QualityGrader::grade(&[quality_grade_event()], 5)
            .expect("violations are independent evidence");
        assert!(report.dimensions.coverage < 100.0);
    }

    #[test]
    fn recommended_gc_interval_matches_grade() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 0).expect("a non-empty window is gradable");
        assert_eq!(
            report.recommended_gc_interval,
            report.grade.recommended_gc_interval()
        );
    }
}
