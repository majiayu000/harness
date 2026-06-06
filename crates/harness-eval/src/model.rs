use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalScenario {
    PrRepair,
    ReadyNoopControl,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvalTarget {
    PullRequest {
        repo: String,
        pr_number: u64,
        base_ref: Option<String>,
        head_ref: Option<String>,
    },
    Issue {
        repo: String,
        issue_number: u64,
    },
    PromptTask {
        task_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeState {
    Unknown,
    Clean,
    Dirty,
    Blocked,
    Behind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Unknown,
    Pending,
    Passing,
    Failing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThreadSnapshot {
    pub id: String,
    pub path: Option<String>,
    pub is_resolved: bool,
    pub is_outdated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFileSnapshot {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSnapshot {
    pub repo: String,
    pub pr_number: u64,
    pub url: Option<String>,
    pub title: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub head_oid: String,
    pub is_draft: bool,
    pub merge_state: MergeState,
    pub check_state: CheckState,
    pub review_decision: Option<ReviewDecision>,
    pub active_unresolved_review_threads: Vec<ReviewThreadSnapshot>,
    #[serde(default = "default_true")]
    pub review_threads_complete: bool,
    pub changed_files: Vec<ChangedFileSnapshot>,
    #[serde(default = "default_true")]
    pub changed_files_complete: bool,
    pub collected_at: String,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeJobSnapshot {
    pub runtime_job_id: String,
    pub state: String,
    pub artifact_count: u64,
    pub terminal_state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub task_id: Option<String>,
    pub workflow_id: Option<String>,
    pub workflow_state: Option<String>,
    pub runtime_jobs: Vec<RuntimeJobSnapshot>,
    pub latest_activity: Option<String>,
    pub terminal_state: Option<String>,
    pub collected_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    Estimated,
    Observed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub agent_invocation_id: Option<String>,
    pub runtime_job_id: Option<String>,
    pub workflow_id: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd_micros: Option<u64>,
    pub token_confidence: Confidence,
    pub cost_confidence: Confidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerKind {
    Human,
    Llm,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerFinding {
    pub path: Option<String>,
    pub summary: String,
    pub severity: FindingSeverity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerJudgment {
    pub reviewer_kind: ReviewerKind,
    pub judged_head_oid: String,
    pub code_quality_score: u8,
    pub trajectory_score: u8,
    pub findings: Vec<ReviewerFinding>,
    pub residual_risks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrRepairEvalInput {
    pub scenario: EvalScenario,
    pub target: EvalTarget,
    pub baseline_pr: PullRequestSnapshot,
    pub final_pr: PullRequestSnapshot,
    pub final_evidence_head_oid: Option<String>,
    pub runtime: Option<RuntimeSnapshot>,
    pub usage: Vec<UsageSnapshot>,
    pub reviewer_judgment: Option<ReviewerJudgment>,
    pub created_unrelated_pr: bool,
    pub scope_violations: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardGateName {
    TargetCorrectness,
    BranchSafety,
    NoUnrelatedPrCreation,
    ScopeContainment,
    HeadChange,
    HeadFreshness,
    RequiredChecks,
    MergeabilityClean,
    ReviewThreadClosure,
    RuntimeArtifactCompleteness,
    ReviewerJudgmentFreshness,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    Fail,
    NotApplicable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvalGrade {
    F,
    D,
    C,
    B,
    A,
}

impl EvalGrade {
    pub fn from_score(score: u8) -> Self {
        match score {
            90..=100 => Self::A,
            80..=89 => Self::B,
            65..=79 => Self::C,
            50..=64 => Self::D,
            _ => Self::F,
        }
    }

    pub fn cap_at(self, cap: Self) -> Self {
        if self.rank() > cap.rank() {
            cap
        } else {
            self
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::F => 0,
            Self::D => 1,
            Self::C => 2,
            Self::B => 3,
            Self::A => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardGateResult {
    pub name: HardGateName,
    pub status: GateStatus,
    pub grade_cap: Option<EvalGrade>,
    pub message: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreDimensionName {
    TaskClassificationAndBaselineEvidence,
    FeedbackDiscoveryAndPrioritization,
    BranchAndPrSafety,
    FixCorrectnessAndScope,
    VerificationAndCurrentHeadGates,
    RuntimeWorkflowBehaviorAndPersistence,
    CostAndTimeEfficiency,
    ReportingAndAttributionQuality,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreComponent {
    pub name: ScoreDimensionName,
    pub points: u8,
    pub max_points: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub task_classification_and_baseline_evidence: ScoreComponent,
    pub feedback_discovery_and_prioritization: ScoreComponent,
    pub branch_and_pr_safety: ScoreComponent,
    pub fix_correctness_and_scope: ScoreComponent,
    pub verification_and_current_head_gates: ScoreComponent,
    pub runtime_workflow_behavior_and_persistence: ScoreComponent,
    pub cost_and_time_efficiency: ScoreComponent,
    pub reporting_and_attribution_quality: ScoreComponent,
}

impl ScoreBreakdown {
    pub fn total(&self) -> u8 {
        self.task_classification_and_baseline_evidence.points
            + self.feedback_discovery_and_prioritization.points
            + self.branch_and_pr_safety.points
            + self.fix_correctness_and_scope.points
            + self.verification_and_current_head_gates.points
            + self.runtime_workflow_behavior_and_persistence.points
            + self.cost_and_time_efficiency.points
            + self.reporting_and_attribution_quality.points
    }

    pub fn max_total(&self) -> u8 {
        self.task_classification_and_baseline_evidence.max_points
            + self.feedback_discovery_and_prioritization.max_points
            + self.branch_and_pr_safety.max_points
            + self.fix_correctness_and_scope.max_points
            + self.verification_and_current_head_gates.max_points
            + self.runtime_workflow_behavior_and_persistence.max_points
            + self.cost_and_time_efficiency.max_points
            + self.reporting_and_attribution_quality.max_points
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualitySnapshot {
    pub scenario: EvalScenario,
    pub target: EvalTarget,
    pub baseline_pr: Option<PullRequestSnapshot>,
    pub final_pr: Option<PullRequestSnapshot>,
    pub runtime: Option<RuntimeSnapshot>,
    pub usage: Vec<UsageSnapshot>,
    pub hard_gates: Vec<HardGateResult>,
    pub objective_score: ScoreBreakdown,
    pub reviewer_judgment: Option<ReviewerJudgment>,
    pub final_score: u8,
    pub final_grade: EvalGrade,
    pub grade_cap: Option<EvalGrade>,
    pub blocker_summary: Vec<String>,
}
