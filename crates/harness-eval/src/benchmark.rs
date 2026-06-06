use crate::model::{EvalGrade, GateStatus, HardGateName, HardGateResult, QualitySnapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrRepairBenchmarkInput {
    pub suite: String,
    pub cases: Vec<PrRepairBenchmarkCase>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrRepairBenchmarkCase {
    pub case_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_case_weight")]
    pub weight: u32,
    pub snapshot: QualitySnapshot,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkConfidence {
    None,
    Low,
    Medium,
    High,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkStatus {
    Empty,
    Failing,
    NeedsReview,
    Passing,
    Excellent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GradeCount {
    pub grade: EvalGrade,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardGateFailureCount {
    pub name: HardGateName,
    pub count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRepairBenchmarkCaseSummary {
    pub case_id: String,
    pub final_score: u8,
    pub effective_score: u8,
    pub final_grade: EvalGrade,
    pub failed_hard_gates: Vec<HardGateName>,
    pub blocker_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRepairBenchmarkSummary {
    pub suite: String,
    pub case_count: u64,
    pub weighted_case_count: u64,
    pub confidence: BenchmarkConfidence,
    pub status: BenchmarkStatus,
    pub capability_score: u8,
    pub average_effective_score: u8,
    pub min_effective_score: u8,
    pub max_effective_score: u8,
    pub excellent_cases: u64,
    pub acceptable_cases: u64,
    pub blocked_cases: u64,
    pub grade_counts: Vec<GradeCount>,
    pub hard_gate_failures: Vec<HardGateFailureCount>,
    pub cases: Vec<PrRepairBenchmarkCaseSummary>,
}

pub fn summarize_pr_repair_benchmark(input: PrRepairBenchmarkInput) -> PrRepairBenchmarkSummary {
    if input.cases.is_empty() {
        return PrRepairBenchmarkSummary {
            suite: input.suite,
            case_count: 0,
            weighted_case_count: 0,
            confidence: BenchmarkConfidence::None,
            status: BenchmarkStatus::Empty,
            capability_score: 0,
            average_effective_score: 0,
            min_effective_score: 0,
            max_effective_score: 0,
            excellent_cases: 0,
            acceptable_cases: 0,
            blocked_cases: 0,
            grade_counts: Vec::new(),
            hard_gate_failures: Vec::new(),
            cases: Vec::new(),
        };
    }

    let mut weighted_score_sum = 0u64;
    let mut weighted_case_count = 0u64;
    let mut min_effective_score = u8::MAX;
    let mut max_effective_score = 0u8;
    let mut excellent_cases = 0u64;
    let mut acceptable_cases = 0u64;
    let mut blocked_cases = 0u64;
    let mut grade_counts = BTreeMap::<u8, (EvalGrade, u64)>::new();
    let mut hard_gate_failures = BTreeMap::<String, (HardGateName, u64)>::new();
    let mut case_summaries = Vec::with_capacity(input.cases.len());

    for case in input.cases {
        let weight = case.weight.max(1) as u64;
        let effective_score = effective_score(&case.snapshot);
        weighted_score_sum += u64::from(effective_score) * weight;
        weighted_case_count += weight;
        min_effective_score = min_effective_score.min(effective_score);
        max_effective_score = max_effective_score.max(effective_score);

        let final_grade = case.snapshot.final_grade;
        grade_counts
            .entry(final_grade.rank())
            .and_modify(|(_, count)| *count += 1)
            .or_insert((final_grade, 1));

        let failed_hard_gates = failed_hard_gates(&case.snapshot.hard_gates);
        for gate in &failed_hard_gates {
            hard_gate_failures
                .entry(format!("{gate:?}"))
                .and_modify(|(_, count)| *count += 1)
                .or_insert((*gate, 1));
        }

        let blocked = !failed_hard_gates.is_empty() || !case.snapshot.blocker_summary.is_empty();
        if blocked {
            blocked_cases += 1;
        }
        if final_grade.rank() >= EvalGrade::B.rank() && !blocked {
            acceptable_cases += 1;
        }
        if final_grade == EvalGrade::A && !blocked {
            excellent_cases += 1;
        }

        case_summaries.push(PrRepairBenchmarkCaseSummary {
            case_id: case.case_id,
            final_score: case.snapshot.final_score,
            effective_score,
            final_grade,
            failed_hard_gates,
            blocker_count: case.snapshot.blocker_summary.len() as u64,
        });
    }

    let average_effective_score = rounded_div(weighted_score_sum, weighted_case_count) as u8;
    let capability_score = ((u16::from(average_effective_score) + 5) / 10).min(10) as u8;
    let case_count = case_summaries.len() as u64;
    let confidence = confidence_for_case_count(case_count);
    let status = benchmark_status(
        confidence,
        capability_score,
        case_count,
        excellent_cases,
        acceptable_cases,
        blocked_cases,
    );

    PrRepairBenchmarkSummary {
        suite: input.suite,
        case_count,
        weighted_case_count,
        confidence,
        status,
        capability_score,
        average_effective_score,
        min_effective_score,
        max_effective_score,
        excellent_cases,
        acceptable_cases,
        blocked_cases,
        grade_counts: grade_counts
            .into_iter()
            .rev()
            .map(|(_, (grade, count))| GradeCount { grade, count })
            .collect(),
        hard_gate_failures: hard_gate_failures
            .into_values()
            .map(|(name, count)| HardGateFailureCount { name, count })
            .collect(),
        cases: case_summaries,
    }
}

fn default_case_weight() -> u32 {
    1
}

fn effective_score(snapshot: &QualitySnapshot) -> u8 {
    snapshot
        .final_score
        .min(grade_score_ceiling(snapshot.final_grade))
}

fn grade_score_ceiling(grade: EvalGrade) -> u8 {
    match grade {
        EvalGrade::A => 100,
        EvalGrade::B => 89,
        EvalGrade::C => 79,
        EvalGrade::D => 64,
        EvalGrade::F => 49,
    }
}

fn failed_hard_gates(gates: &[HardGateResult]) -> Vec<HardGateName> {
    gates
        .iter()
        .filter(|gate| gate.status == GateStatus::Fail)
        .map(|gate| gate.name)
        .collect()
}

fn rounded_div(numerator: u64, denominator: u64) -> u64 {
    numerator
        .saturating_add(denominator / 2)
        .checked_div(denominator)
        .unwrap_or(0)
}

fn confidence_for_case_count(case_count: u64) -> BenchmarkConfidence {
    match case_count {
        0 => BenchmarkConfidence::None,
        1..=2 => BenchmarkConfidence::Low,
        3..=4 => BenchmarkConfidence::Medium,
        _ => BenchmarkConfidence::High,
    }
}

fn benchmark_status(
    confidence: BenchmarkConfidence,
    capability_score: u8,
    case_count: u64,
    excellent_cases: u64,
    acceptable_cases: u64,
    blocked_cases: u64,
) -> BenchmarkStatus {
    if case_count == 0 {
        return BenchmarkStatus::Empty;
    }
    if blocked_cases > 0 || capability_score < 7 {
        return BenchmarkStatus::Failing;
    }
    if confidence == BenchmarkConfidence::Low || acceptable_cases < case_count {
        return BenchmarkStatus::NeedsReview;
    }
    if capability_score == 10 && excellent_cases == case_count {
        return BenchmarkStatus::Excellent;
    }
    BenchmarkStatus::Passing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        EvalScenario, EvalTarget, GateStatus, HardGateResult, ScoreBreakdown, ScoreComponent,
        ScoreDimensionName,
    };

    #[test]
    fn empty_benchmark_has_no_confidence() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "empty".to_string(),
            cases: Vec::new(),
        });

        assert_eq!(summary.confidence, BenchmarkConfidence::None);
        assert_eq!(summary.status, BenchmarkStatus::Empty);
        assert_eq!(summary.capability_score, 0);
    }

    #[test]
    fn single_perfect_case_is_low_confidence_not_excellent() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "smoke".to_string(),
            cases: vec![case("case-1", snapshot(100, EvalGrade::A, Vec::new()))],
        });

        assert_eq!(summary.capability_score, 10);
        assert_eq!(summary.confidence, BenchmarkConfidence::Low);
        assert_eq!(summary.status, BenchmarkStatus::NeedsReview);
        assert_eq!(summary.excellent_cases, 1);
    }

    #[test]
    fn grade_cap_lowers_effective_score() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "caps".to_string(),
            cases: vec![case("case-1", snapshot(100, EvalGrade::C, Vec::new()))],
        });

        assert_eq!(summary.average_effective_score, 79);
        assert_eq!(summary.capability_score, 8);
        assert_eq!(summary.cases[0].effective_score, 79);
    }

    #[test]
    fn three_clean_a_cases_are_excellent() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "regression".to_string(),
            cases: vec![
                case("case-1", snapshot(98, EvalGrade::A, Vec::new())),
                case("case-2", snapshot(99, EvalGrade::A, Vec::new())),
                case("case-3", snapshot(100, EvalGrade::A, Vec::new())),
            ],
        });

        assert_eq!(summary.confidence, BenchmarkConfidence::Medium);
        assert_eq!(summary.status, BenchmarkStatus::Excellent);
        assert_eq!(summary.capability_score, 10);
        assert_eq!(summary.blocked_cases, 0);
    }

    #[test]
    fn failed_gate_blocks_and_counts_failure_kind() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "blocked".to_string(),
            cases: vec![case(
                "case-1",
                snapshot(92, EvalGrade::C, vec![HardGateName::ReviewThreadClosure]),
            )],
        });

        assert_eq!(summary.status, BenchmarkStatus::Failing);
        assert_eq!(summary.blocked_cases, 1);
        assert_eq!(
            summary.hard_gate_failures,
            vec![HardGateFailureCount {
                name: HardGateName::ReviewThreadClosure,
                count: 1,
            }]
        );
    }

    #[test]
    fn grade_counts_sort_high_to_low() {
        let summary = summarize_pr_repair_benchmark(PrRepairBenchmarkInput {
            suite: "grades".to_string(),
            cases: vec![
                case("case-1", snapshot(49, EvalGrade::F, Vec::new())),
                case("case-2", snapshot(89, EvalGrade::B, Vec::new())),
                case("case-3", snapshot(100, EvalGrade::A, Vec::new())),
            ],
        });

        let grades: Vec<_> = summary
            .grade_counts
            .into_iter()
            .map(|count| count.grade)
            .collect();

        assert_eq!(grades, vec![EvalGrade::A, EvalGrade::B, EvalGrade::F]);
    }

    fn case(case_id: &str, snapshot: QualitySnapshot) -> PrRepairBenchmarkCase {
        PrRepairBenchmarkCase {
            case_id: case_id.to_string(),
            tags: Vec::new(),
            weight: 1,
            snapshot,
        }
    }

    fn snapshot(
        final_score: u8,
        final_grade: EvalGrade,
        failed_gates: Vec<HardGateName>,
    ) -> QualitySnapshot {
        QualitySnapshot {
            scenario: EvalScenario::PrRepair,
            run_mode: crate::model::EvalRunMode::LiveRun,
            target: EvalTarget::PullRequest {
                repo: "owner/repo".to_string(),
                pr_number: 7,
                base_ref: Some("main".to_string()),
                head_ref: Some("feature".to_string()),
            },
            baseline_pr: None,
            final_pr: None,
            runtime: None,
            usage: Vec::new(),
            hard_gates: failed_gates
                .into_iter()
                .map(|name| HardGateResult {
                    name,
                    status: GateStatus::Fail,
                    grade_cap: Some(EvalGrade::C),
                    message: "failed gate".to_string(),
                })
                .collect(),
            objective_score: score_breakdown(),
            reviewer_judgment: None,
            final_score,
            final_grade,
            grade_cap: None,
            blocker_summary: Vec::new(),
        }
    }

    fn score_breakdown() -> ScoreBreakdown {
        ScoreBreakdown {
            task_classification_and_baseline_evidence: test_component(
                ScoreDimensionName::TaskClassificationAndBaselineEvidence,
            ),
            feedback_discovery_and_prioritization: test_component(
                ScoreDimensionName::FeedbackDiscoveryAndPrioritization,
            ),
            branch_and_pr_safety: test_component(ScoreDimensionName::BranchAndPrSafety),
            fix_correctness_and_scope: test_component(ScoreDimensionName::FixCorrectnessAndScope),
            verification_and_current_head_gates: test_component(
                ScoreDimensionName::VerificationAndCurrentHeadGates,
            ),
            runtime_workflow_behavior_and_persistence: test_component(
                ScoreDimensionName::RuntimeWorkflowBehaviorAndPersistence,
            ),
            cost_and_time_efficiency: test_component(ScoreDimensionName::CostAndTimeEfficiency),
            reporting_and_attribution_quality: test_component(
                ScoreDimensionName::ReportingAndAttributionQuality,
            ),
        }
    }

    fn test_component(name: ScoreDimensionName) -> ScoreComponent {
        ScoreComponent {
            name,
            points: 0,
            max_points: 0,
        }
    }
}
