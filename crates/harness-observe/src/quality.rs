use harness_core::{Decision, Event, Grade};
use serde::{Deserialize, Serialize};

/// All inputs required to compute a quality grade.
#[derive(Debug, Clone)]
pub struct QualityInput {
    /// Hook events collected during the task lifecycle.
    pub events: Vec<Event>,
    /// Number of rule violations found by the rule_enforcer scan.
    pub violation_count: usize,
    /// Whether the post-validation test run passed (cargo test / go test / etc.).
    pub test_passed: bool,
    /// Number of clippy (or equivalent linter) warnings emitted during validation.
    pub clippy_warnings: usize,
    /// Number of Agent Review fix cycles consumed (fewer = better quality first pass).
    pub review_rounds: u32,
    /// Number of files changed in the task diff (used to compute diff complexity).
    pub changed_files: usize,
    /// Average number of diff lines per changed file.
    pub avg_diff_lines: usize,
    /// Whether the PR/commit has a non-empty description body.
    pub has_pr_description: bool,
    /// Whether the PR body contains a linked issue reference.
    pub has_linked_issue: bool,
}

impl Default for QualityInput {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            violation_count: 0,
            test_passed: true,
            clippy_warnings: 0,
            review_rounds: 0,
            changed_files: 0,
            avg_diff_lines: 0,
            has_pr_description: false,
            has_linked_issue: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub score: f64,
    pub grade: Grade,
    pub dimensions: QualityDimensions,
    pub recommended_gc_interval: std::time::Duration,
}

/// Individual quality dimension scores (each in the range 0–100).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityDimensions {
    /// Proportion of test runs that passed (100 = all passed, 0 = all failed).
    pub test_pass_rate: f64,
    /// Linter health: 100 minus a penalty scaled by warning count.
    pub clippy_warnings: f64,
    /// Review-cycle efficiency: 100 when zero fix rounds were needed.
    pub review_rounds: f64,
    /// Rule compliance: inverse of violation density (mirrors old `coverage`).
    pub violation_count: f64,
    /// Change-set size: smaller, focused diffs score higher.
    pub diff_complexity: f64,
    /// PR metadata completeness: description and linked-issue presence.
    pub pr_completeness: f64,
    /// Event stability: low ratio of blocked or escalated decisions across all hooks.
    pub stability: f64,
}

/// Maximum number of clippy warnings before the score hits zero.
const CLIPPY_WARNINGS_CAP: f64 = 10.0;
/// Maximum number of review-fix rounds before the score hits zero.
const REVIEW_ROUNDS_CAP: f64 = 5.0;
/// Maximum number of violations before the score hits zero.
const VIOLATION_COUNT_CAP: f64 = 100.0;
/// Divisor for the diff-complexity index (changed_files * avg_diff_lines / N).
const DIFF_COMPLEXITY_DIVISOR: f64 = 500.0;

/// Scoring weights — must sum to 1.0.
const WEIGHT_TEST_PASS_RATE: f64 = 0.25;
const WEIGHT_CLIPPY: f64 = 0.15;
const WEIGHT_REVIEW: f64 = 0.15;
const WEIGHT_VIOLATION: f64 = 0.15;
const WEIGHT_DIFF_COMPLEXITY: f64 = 0.10;
const WEIGHT_PR_COMPLETENESS: f64 = 0.10;
const WEIGHT_STABILITY: f64 = 0.10;

pub struct QualityGrader;

impl QualityGrader {
    /// Compute a `QualityReport` from structured input covering all quality dimensions.
    pub fn grade(input: &QualityInput) -> QualityReport {
        let events = &input.events;
        let total = events.len().max(1) as f64;

        // Stability (0.10): ratio of blocked or escalated events to total events.
        // Counts ALL Block/Escalate decisions, not only security-labelled ones, so that
        // a history full of blocked pre_tool_use events correctly reduces quality.
        let stability_issues = events
            .iter()
            .filter(|e| matches!(e.decision, Decision::Block | Decision::Escalate))
            .count() as f64;
        let stability = (1.0 - stability_issues / total) * 100.0;

        // Test pass rate (0.25): binary pass/fail from post-validation.
        let test_pass_rate = if input.test_passed { 100.0 } else { 0.0 };

        // Clippy warnings (0.15): penalise each warning up to a cap.
        let clippy = (1.0 - (input.clippy_warnings as f64 / CLIPPY_WARNINGS_CAP).min(1.0)) * 100.0;

        // Review rounds (0.15): each extra fix cycle lowers the score (cap at cap).
        let review = (1.0 - (input.review_rounds as f64 / REVIEW_ROUNDS_CAP).min(1.0)) * 100.0;

        // Violation count (0.15): inverse of violation density.
        let violation = if input.violation_count == 0 {
            100.0
        } else {
            (1.0 - (input.violation_count as f64 / VIOLATION_COUNT_CAP).min(1.0)) * 100.0
        };

        // Diff complexity (0.10): penalise large, sprawling diffs.
        // Complexity index = changed_files * avg_diff_lines / DIVISOR, capped at 1.
        let complexity_index = (input.changed_files as f64 * input.avg_diff_lines as f64
            / DIFF_COMPLEXITY_DIVISOR)
            .min(1.0);
        let diff_complexity = (1.0 - complexity_index) * 100.0;

        // PR completeness (0.10): one point each for description and linked issue.
        let completeness_points = input.has_pr_description as u8 + input.has_linked_issue as u8;
        let pr_completeness = (completeness_points as f64 / 2.0) * 100.0;

        // Weighted score.
        let score = test_pass_rate * WEIGHT_TEST_PASS_RATE
            + clippy * WEIGHT_CLIPPY
            + review * WEIGHT_REVIEW
            + violation * WEIGHT_VIOLATION
            + diff_complexity * WEIGHT_DIFF_COMPLEXITY
            + pr_completeness * WEIGHT_PR_COMPLETENESS
            + stability * WEIGHT_STABILITY;

        let grade = Grade::from_score(score);

        QualityReport {
            score,
            grade,
            dimensions: QualityDimensions {
                test_pass_rate,
                clippy_warnings: clippy,
                review_rounds: review,
                violation_count: violation,
                diff_complexity,
                pr_completeness,
                stability,
            },
            recommended_gc_interval: grade.recommended_gc_interval(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, Grade, SessionId};

    fn pass_event() -> Event {
        Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass)
    }

    fn block_event(hook: &str) -> Event {
        Event::new(SessionId::new(), hook, "Edit", Decision::Block)
    }

    fn base_input() -> QualityInput {
        QualityInput {
            events: (0..10).map(|_| pass_event()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn grade_perfect_input_is_a() {
        let input = QualityInput {
            events: (0..10).map(|_| pass_event()).collect(),
            test_passed: true,
            clippy_warnings: 0,
            review_rounds: 0,
            violation_count: 0,
            changed_files: 0,
            avg_diff_lines: 0,
            has_pr_description: true,
            has_linked_issue: true,
        };
        let report = QualityGrader::grade(&input);
        assert_eq!(report.grade, Grade::A);
        assert!((report.score - 100.0).abs() < 0.1);
    }

    #[test]
    fn grade_degrades_with_violations() {
        let mut input = base_input();
        input.violation_count = 50;
        let report = QualityGrader::grade(&input);
        assert!(report.dimensions.violation_count < 100.0);
        assert!(report.score < 100.0);
    }

    #[test]
    fn grade_degrades_with_many_blocks() {
        let input = QualityInput {
            events: (0..10).map(|_| block_event("security_check")).collect(),
            ..Default::default()
        };
        let report = QualityGrader::grade(&input);
        assert!(report.dimensions.stability < 100.0);
    }

    #[test]
    fn grade_degrades_with_non_security_blocks() {
        // pre_tool_use blocks have no "security" in hook name; must still lower quality.
        let input = QualityInput {
            events: (0..10).map(|_| block_event("pre_tool_use")).collect(),
            ..Default::default()
        };
        let report = QualityGrader::grade(&input);
        assert!(
            report.dimensions.stability < 100.0,
            "non-security Block events must reduce the stability dimension"
        );
    }

    #[test]
    fn grade_degrades_with_escalations() {
        let escalate = |_| Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Escalate);
        let input = QualityInput {
            events: (0..10).map(escalate).collect(),
            ..Default::default()
        };
        let report = QualityGrader::grade(&input);
        assert!(
            report.dimensions.stability < 100.0,
            "Escalate events must reduce the stability dimension"
        );
    }

    #[test]
    fn grade_empty_events_returns_report() {
        let report = QualityGrader::grade(&QualityInput::default());
        assert!(report.score >= 0.0);
    }

    #[test]
    fn recommended_gc_interval_matches_grade() {
        let report = QualityGrader::grade(&QualityInput::default());
        assert_eq!(
            report.recommended_gc_interval,
            report.grade.recommended_gc_interval()
        );
    }

    #[test]
    fn test_failure_lowers_score_significantly() {
        let passing = QualityGrader::grade(&base_input());
        let mut failing_input = base_input();
        failing_input.test_passed = false;
        let failing = QualityGrader::grade(&failing_input);
        // test_pass_rate has weight 0.25, so failing should drop score by 25 pts.
        assert!(passing.score - failing.score > 20.0);
        assert_eq!(failing.dimensions.test_pass_rate, 0.0);
    }

    #[test]
    fn clippy_warnings_lower_score() {
        let clean = QualityGrader::grade(&base_input());
        let mut noisy_input = base_input();
        noisy_input.clippy_warnings = 10;
        let noisy = QualityGrader::grade(&noisy_input);
        assert!(clean.score > noisy.score);
        assert_eq!(noisy.dimensions.clippy_warnings, 0.0);
    }

    #[test]
    fn review_rounds_lower_score() {
        let clean = QualityGrader::grade(&base_input());
        let mut rounds_input = base_input();
        rounds_input.review_rounds = 5;
        let rounds = QualityGrader::grade(&rounds_input);
        assert!(clean.score > rounds.score);
        assert_eq!(rounds.dimensions.review_rounds, 0.0);
    }

    #[test]
    fn pr_completeness_raises_score() {
        let incomplete = QualityGrader::grade(&base_input());
        let complete = QualityGrader::grade(&QualityInput {
            events: (0..10).map(|_| pass_event()).collect(),
            has_pr_description: true,
            has_linked_issue: true,
            ..Default::default()
        });
        assert!(complete.score > incomplete.score);
        assert_eq!(complete.dimensions.pr_completeness, 100.0);
    }

    #[test]
    fn diff_complexity_penalises_large_diffs() {
        let simple = QualityGrader::grade(&base_input());
        let complex = QualityGrader::grade(&QualityInput {
            events: (0..10).map(|_| pass_event()).collect(),
            changed_files: 10,
            avg_diff_lines: 100,
            ..Default::default()
        });
        // 10 * 100 / 500 = 2.0 → capped at 1.0 → diff_complexity = 0.0
        assert!(simple.score > complex.score);
        assert_eq!(complex.dimensions.diff_complexity, 0.0);
    }

    #[test]
    fn weights_sum_to_one_implicitly() {
        // All dimensions at 100 should yield score == 100.
        let input = QualityInput {
            events: vec![pass_event()],
            test_passed: true,
            clippy_warnings: 0,
            review_rounds: 0,
            violation_count: 0,
            changed_files: 0,
            avg_diff_lines: 0,
            has_pr_description: true,
            has_linked_issue: true,
        };
        let report = QualityGrader::grade(&input);
        assert!(
            (report.score - 100.0).abs() < 0.001,
            "expected score ~100, got {}",
            report.score
        );
    }
}
