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

impl QualityGrader {
    /// Grades a window of events. Returns `None` when the window carries no
    /// signal at all — no events and no violations.
    ///
    /// Every dimension below divides by `events.len().max(1)`, so an empty
    /// window would otherwise score a perfect 100 / Grade::A. That is a
    /// fabricated verdict: "nothing was observed" is not "everything was
    /// clean", and callers that feed GC cadence or dashboards must be able to
    /// tell the two apart.
    pub fn grade(events: &[Event], violation_count: usize) -> Option<QualityReport> {
        if events.is_empty() && violation_count == 0 {
            return None;
        }

        let total = events.len().max(1) as f64;

        // Security: ratio of security-related blocks to total events
        let security_issues = events
            .iter()
            .filter(|e| matches!(e.decision, Decision::Block) && e.hook.contains("security"))
            .count() as f64;
        let security = (1.0 - security_issues / total) * 100.0;

        // Stability: ratio of non-failed events
        let failures = events
            .iter()
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
        let slow_ops = events
            .iter()
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

    #[test]
    fn grade_perfect_events_no_violations() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 0).expect("non-empty window is gradeable");
        assert_eq!(report.grade, Grade::A);
        assert!(report.score >= 90.0);
    }

    #[test]
    fn grade_degrades_with_violations() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 50).expect("non-empty window is gradeable");
        assert!(report.score < 100.0);
        assert!(report.dimensions.coverage < 100.0);
    }

    #[test]
    fn grade_degrades_with_many_blocks() {
        let events: Vec<Event> = (0..10).map(|_| block_event("security_check")).collect();
        let report = QualityGrader::grade(&events, 0).expect("non-empty window is gradeable");
        assert!(report.dimensions.stability < 100.0);
    }

    #[test]
    fn grade_empty_window_has_no_verdict() {
        assert!(
            QualityGrader::grade(&[], 0).is_none(),
            "an empty window must not fabricate a perfect score"
        );
    }

    #[test]
    fn grade_violations_without_events_still_produces_a_verdict() {
        let report =
            QualityGrader::grade(&[], 25).expect("violations are signal even without events");
        assert!(report.dimensions.coverage < 100.0);
    }

    #[test]
    fn recommended_gc_interval_matches_grade() {
        let events: Vec<Event> = (0..10).map(|_| pass_event()).collect();
        let report = QualityGrader::grade(&events, 0).expect("non-empty window is gradeable");
        assert_eq!(
            report.recommended_gc_interval,
            report.grade.recommended_gc_interval()
        );
    }
}
