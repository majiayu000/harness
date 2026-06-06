use harness_eval::{
    EvalGrade, EvalScenario, EvalTarget, GateStatus, HardGateName, HardGateResult, QualitySnapshot,
    ScoreBreakdown, ScoreComponent, ScoreDimensionName,
};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn benchmark_cli_aggregates_snapshot_cases() {
    let temp_dir = TempDir::new("pr-repair-benchmark-cli");

    let ready_noop = temp_dir.path().join("ready_noop.json");
    let clean_fix = temp_dir.path().join("clean_fix.json");
    let blocked_threads = temp_dir.path().join("blocked_threads.json");
    let output = temp_dir.path().join("benchmark_summary.json");

    write_snapshot(
        &ready_noop,
        &fixture_snapshot("ready-noop", 100, EvalGrade::A, None),
    );
    write_snapshot(
        &clean_fix,
        &fixture_snapshot("clean-fix", 95, EvalGrade::A, None),
    );
    write_snapshot(
        &blocked_threads,
        &fixture_snapshot(
            "blocked-threads",
            75,
            EvalGrade::C,
            Some(HardGateName::ReviewThreadClosure),
        ),
    );

    let status = Command::new(env!("CARGO_BIN_EXE_score_pr_repair_benchmark"))
        .arg("--suite")
        .arg("three-case-smoke")
        .arg("--case")
        .arg(case_arg("ready-noop", &ready_noop))
        .arg("--case")
        .arg(case_arg("clean-fix", &clean_fix))
        .arg("--snapshot")
        .arg(&blocked_threads)
        .arg("--output")
        .arg(&output)
        .status()
        .expect("run benchmark binary");
    assert!(status.success(), "benchmark binary should succeed");

    let summary: Value = serde_json::from_str(&fs::read_to_string(&output).expect("read summary"))
        .expect("parse summary");

    assert_eq!(summary["suite"], "three-case-smoke");
    assert_eq!(summary["case_count"], 3);
    assert_eq!(summary["confidence"], "medium");
    assert_eq!(summary["capability_score"], 9);
    assert_eq!(summary["status"], "failing");
    assert_eq!(summary["blocked_cases"], 1);
    assert_eq!(
        summary["hard_gate_failures"][0]["name"],
        "review_thread_closure"
    );
    assert_eq!(summary["hard_gate_failures"][0]["count"], 1);
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = unique_temp_path(name);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
}

fn case_arg(case_id: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(case_id);
    arg.push("=");
    arg.push(path.as_os_str());
    arg
}

fn write_snapshot(path: &Path, snapshot: &QualitySnapshot) {
    let body = serde_json::to_string_pretty(snapshot).expect("serialize snapshot");
    fs::write(path, format!("{body}\n")).expect("write snapshot");
}

fn fixture_snapshot(
    suffix: &str,
    final_score: u8,
    final_grade: EvalGrade,
    failed_gate: Option<HardGateName>,
) -> QualitySnapshot {
    QualitySnapshot {
        scenario: EvalScenario::PrRepair,
        target: EvalTarget::PullRequest {
            repo: "owner/repo".to_string(),
            pr_number: 42,
            base_ref: Some("main".to_string()),
            head_ref: Some(format!("repair-{suffix}")),
        },
        baseline_pr: None,
        final_pr: None,
        runtime: None,
        usage: Vec::new(),
        hard_gates: hard_gates(failed_gate),
        objective_score: fixture_score_breakdown(final_score),
        reviewer_judgment: None,
        final_score,
        final_grade,
        grade_cap: failed_gate.map(|_| EvalGrade::C),
        blocker_summary: failed_gate
            .map(|gate| vec![format!("{gate:?} blocked readiness")])
            .unwrap_or_default(),
    }
}

fn hard_gates(failed_gate: Option<HardGateName>) -> Vec<HardGateResult> {
    [
        HardGateName::TargetCorrectness,
        HardGateName::ReviewThreadClosure,
    ]
    .into_iter()
    .map(|name| HardGateResult {
        name,
        status: if Some(name) == failed_gate {
            GateStatus::Fail
        } else {
            GateStatus::Pass
        },
        grade_cap: if Some(name) == failed_gate {
            Some(EvalGrade::C)
        } else {
            None
        },
        message: format!("{name:?} gate"),
    })
    .collect()
}

fn fixture_score_breakdown(final_score: u8) -> ScoreBreakdown {
    let base = final_score / 8;
    let remainder = final_score % 8;
    let points = |index: u8| base + u8::from(index < remainder);

    ScoreBreakdown {
        task_classification_and_baseline_evidence: score_component(
            ScoreDimensionName::TaskClassificationAndBaselineEvidence,
            points(0),
        ),
        feedback_discovery_and_prioritization: score_component(
            ScoreDimensionName::FeedbackDiscoveryAndPrioritization,
            points(1),
        ),
        branch_and_pr_safety: score_component(ScoreDimensionName::BranchAndPrSafety, points(2)),
        fix_correctness_and_scope: score_component(
            ScoreDimensionName::FixCorrectnessAndScope,
            points(3),
        ),
        verification_and_current_head_gates: score_component(
            ScoreDimensionName::VerificationAndCurrentHeadGates,
            points(4),
        ),
        runtime_workflow_behavior_and_persistence: score_component(
            ScoreDimensionName::RuntimeWorkflowBehaviorAndPersistence,
            points(5),
        ),
        cost_and_time_efficiency: score_component(
            ScoreDimensionName::CostAndTimeEfficiency,
            points(6),
        ),
        reporting_and_attribution_quality: score_component(
            ScoreDimensionName::ReportingAndAttributionQuality,
            points(7),
        ),
    }
}

fn score_component(name: ScoreDimensionName, points: u8) -> ScoreComponent {
    ScoreComponent {
        name,
        points,
        max_points: 100,
    }
}
