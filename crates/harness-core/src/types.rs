use crate::db::DbSerializable;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

// === ID Types (Newtype pattern) ===

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            #[allow(clippy::should_implement_trait)]
            pub fn from_str(s: &str) -> Self {
                Self(s.to_string())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(ThreadId);
define_id!(TurnId);
define_id!(AgentId);
define_id!(SignalId);
define_id!(ProjectId);
define_id!(DraftId);
define_id!(SkillId);
define_id!(ExecPlanId);
define_id!(RuleId);
define_id!(GuardId);
define_id!(SessionId);
define_id!(EventId);
define_id!(TaskId);
define_id!(ExternalSignalId);

// === Thread ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub project_root: PathBuf,
    pub turns: Vec<Turn>,
    pub status: ThreadStatus,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Active,
    Archived,
}

impl DbSerializable for ThreadStatus {
    fn to_db_str(&self) -> &'static str {
        match self {
            ThreadStatus::Idle => "idle",
            ThreadStatus::Active => "active",
            ThreadStatus::Archived => "archived",
        }
    }

    fn from_db_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "idle" => Ok(ThreadStatus::Idle),
            "active" => Ok(ThreadStatus::Active),
            "archived" => Ok(ThreadStatus::Archived),
            _ => anyhow::bail!("unknown thread status `{s}`"),
        }
    }
}

impl AsRef<str> for ThreadStatus {
    fn as_ref(&self) -> &str {
        self.to_db_str()
    }
}

impl std::str::FromStr for ThreadStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_db_str(s)
    }
}

impl Thread {
    pub fn new(project_root: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id: ThreadId::new(),
            project_root,
            turns: Vec::new(),
            status: ThreadStatus::Idle,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            created_at: now,
            updated_at: now,
        }
    }
}

// === Turn ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub items: Vec<Item>,
    pub status: TurnStatus,
    pub agent_id: AgentId,
    pub token_usage: TokenUsage,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

impl Turn {
    pub fn new(thread_id: ThreadId, agent_id: AgentId) -> Self {
        Self {
            id: TurnId::new(),
            thread_id,
            items: Vec::new(),
            status: TurnStatus::Running,
            agent_id,
            token_usage: TokenUsage::default(),
            started_at: Utc::now(),
            completed_at: None,
        }
    }

    pub fn complete(&mut self) {
        self.status = TurnStatus::Completed;
        self.completed_at = Some(Utc::now());
    }

    pub fn cancel(&mut self) {
        self.status = TurnStatus::Cancelled;
        self.completed_at = Some(Utc::now());
    }

    pub fn fail(&mut self) {
        self.status = TurnStatus::Failed;
        self.completed_at = Some(Utc::now());
    }
}

// === Item ===

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    UserMessage {
        content: String,
    },
    AgentReasoning {
        content: String,
    },
    ShellCommand {
        command: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    FileEdit {
        path: PathBuf,
        before: String,
        after: String,
    },
    FileRead {
        path: PathBuf,
        content: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
        output: Option<serde_json::Value>,
    },
    ApprovalRequest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        action: String,
        approved: Option<bool>,
    },
    Error {
        code: i32,
        message: String,
    },
}

// === Signal ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    RepeatedWarn,
    ChronicBlock,
    HotFiles,
    SlowSessions,
    WarnEscalation,
    LinterViolations,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationType {
    Guard,
    Rule,
    Hook,
    Skill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: SignalId,
    pub signal_type: SignalType,
    pub project_id: ProjectId,
    pub details: serde_json::Value,
    pub remediation: RemediationType,
    pub detected_at: DateTime<Utc>,
}

impl Signal {
    pub fn new(
        signal_type: SignalType,
        project_id: ProjectId,
        details: serde_json::Value,
        remediation: RemediationType,
    ) -> Self {
        Self {
            id: SignalId::new(),
            signal_type,
            project_id,
            details,
            remediation,
            detected_at: Utc::now(),
        }
    }
}

// === ExternalSignal ===

/// A signal pushed from an external system (e.g. GitHub CI, PagerDuty).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSignal {
    pub id: ExternalSignalId,
    /// Source system identifier, e.g. "github", "pagerduty", "generic".
    pub source: String,
    pub severity: Severity,
    pub payload: serde_json::Value,
    pub received_at: DateTime<Utc>,
}

impl ExternalSignal {
    pub fn new(source: String, severity: Severity, payload: serde_json::Value) -> Self {
        Self {
            id: ExternalSignalId::new(),
            source,
            severity,
            payload,
            received_at: Utc::now(),
        }
    }
}

// === Event ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Pass,
    Warn,
    Block,
    Gate,
    Escalate,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub ts: DateTime<Utc>,
    pub session_id: SessionId,
    pub hook: String,
    pub tool: String,
    pub decision: Decision,
    pub reason: Option<String>,
    pub detail: Option<String>,
    /// Full content payload for auditing (e.g. raw reviewer output for agent_review events).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
    pub duration_ms: Option<u64>,
}

impl Event {
    pub fn new(session_id: SessionId, hook: &str, tool: &str, decision: Decision) -> Self {
        Self {
            id: EventId::new(),
            ts: Utc::now(),
            session_id,
            hook: hook.to_string(),
            tool: tool.to_string(),
            decision,
            reason: None,
            detail: None,
            content: None,
            duration_ms: None,
        }
    }
}

// === Rule / Guard ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Security,
    Stability,
    Style,
    Performance,
    DataConsistency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Java,
    CSharp,
    Ruby,
    Common,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub rule_id: RuleId,
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
    pub severity: Severity,
}

// === Auto-fix ===

/// Outcome of a single auto-fix attempt for one violation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoFixAttempt {
    pub rule_id: RuleId,
    pub file: PathBuf,
    pub line: Option<usize>,
    /// Whether the fix_pattern was successfully applied to the file.
    pub applied: bool,
    /// Whether the violation is absent after the post-fix re-scan.
    pub resolved: bool,
}

/// Summary returned by `RuleEngine::scan_and_fix`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoFixReport {
    pub attempts: Vec<AutoFixAttempt>,
    /// Number of violations where the fix_pattern was applied to the file.
    pub fixed_count: usize,
    /// Violations remaining after the auto-fix re-scan.
    pub residual_violations: Vec<Violation>,
}

// === Skill ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLocation {
    Repo,
    User,
    Admin,
    System,
}

// === Draft ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Pending,
    Adopted,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    Guard,
    Rule,
    Hook,
    Skill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub artifact_type: ArtifactType,
    pub target_path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: DraftId,
    pub status: DraftStatus,
    pub signal: Signal,
    pub artifacts: Vec<Artifact>,
    pub rationale: String,
    pub validation: String,
    pub generated_at: DateTime<Utc>,
    pub agent_model: String,
}

// === Project ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub root: PathBuf,
    pub languages: Vec<Language>,
    pub name: String,
}

impl Project {
    pub fn from_path(root: PathBuf) -> Self {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        Self {
            id: ProjectId::new(),
            root,
            languages: Vec::new(),
            name,
        }
    }
}

// === Capability ===

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Read,
    Write,
    Execute,
    Network,
    Approve,
}

// === Context Item ===

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextItem {
    Rule { id: String, content: String },
    Skill { id: String, content: String },
    History { turn_id: TurnId, summary: String },
    File { path: PathBuf, content: String },
    AgentsMd { content: String },
}

// === Quality ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grade {
    A,
    B,
    C,
    D,
}

impl Grade {
    pub fn from_score(score: f64) -> Self {
        match score as u64 {
            90..=100 => Grade::A,
            70..=89 => Grade::B,
            50..=69 => Grade::C,
            _ => Grade::D,
        }
    }

    pub fn recommended_gc_interval(&self) -> Duration {
        match self {
            Grade::A => Duration::from_secs(7 * 24 * 3600),
            Grade::B => Duration::from_secs(3 * 24 * 3600),
            Grade::C => Duration::from_secs(24 * 3600),
            Grade::D => Duration::from_secs(3600),
        }
    }
}

// === ExecPlan Status ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecPlanStatus {
    Draft,
    Active,
    Completed,
    Abandoned,
}

// === Budget ===

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetTier {
    XHigh,
    High,
    Medium,
}

/// Phase of task execution used to select the appropriate model via ReasoningBudget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPhase {
    /// Initial planning and analysis (uses high-reasoning model).
    Planning,
    /// Core implementation work (uses balanced model).
    Execution,
    /// Validation and review (uses high-reasoning model).
    Validation,
}

/// Per-phase model selection configuration.
///
/// Maps each `ExecutionPhase` to a `BudgetTier`, then maps tiers to model identifiers.
/// When configured on `ClaudeCodeAgent`, the model is selected by phase rather than
/// the flat `default_model`. Falls back to `req.model` or `default_model` when no
/// `execution_phase` is set on the `AgentRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningBudget {
    /// Tier used for the Planning phase (default: XHigh).
    #[serde(default = "default_planning_tier")]
    pub planning_tier: BudgetTier,
    /// Tier used for the Execution phase (default: High).
    #[serde(default = "default_execution_tier")]
    pub execution_tier: BudgetTier,
    /// Tier used for the Validation phase (default: XHigh).
    #[serde(default = "default_validation_tier")]
    pub validation_tier: BudgetTier,
    /// Model identifier for XHigh tier (default: opus).
    #[serde(default = "default_xhigh_model")]
    pub xhigh_model: String,
    /// Model identifier for High tier (default: sonnet).
    #[serde(default = "default_high_model")]
    pub high_model: String,
    /// Model identifier for Medium tier (default: haiku).
    #[serde(default = "default_medium_model")]
    pub medium_model: String,
}

fn default_planning_tier() -> BudgetTier {
    BudgetTier::XHigh
}
fn default_execution_tier() -> BudgetTier {
    BudgetTier::High
}
fn default_validation_tier() -> BudgetTier {
    BudgetTier::XHigh
}
fn default_xhigh_model() -> String {
    "claude-opus-4-20250514".to_string()
}
fn default_high_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}
fn default_medium_model() -> String {
    "claude-haiku-4-20250514".to_string()
}

impl Default for ReasoningBudget {
    fn default() -> Self {
        Self {
            planning_tier: default_planning_tier(),
            execution_tier: default_execution_tier(),
            validation_tier: default_validation_tier(),
            xhigh_model: default_xhigh_model(),
            high_model: default_high_model(),
            medium_model: default_medium_model(),
        }
    }
}

impl ReasoningBudget {
    /// Returns the model identifier for the given execution phase.
    pub fn model_for_phase(&self, phase: ExecutionPhase) -> &str {
        let tier = match phase {
            ExecutionPhase::Planning => self.planning_tier,
            ExecutionPhase::Execution => self.execution_tier,
            ExecutionPhase::Validation => self.validation_tier,
        };
        match tier {
            BudgetTier::XHigh => &self.xhigh_model,
            BudgetTier::High => &self.high_model,
            BudgetTier::Medium => &self.medium_model,
        }
    }
}

// === Event Filters ===

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventFilters {
    pub session_id: Option<SessionId>,
    pub hook: Option<String>,
    /// Filter by `Event.tool` (e.g. rule id for `rule_check` events).
    pub tool: Option<String>,
    pub decision: Option<Decision>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
    /// Whether to load the `content` column (large blobs; off by default).
    /// Only enable on internal audit paths — never on public-facing queries.
    #[serde(default)]
    pub include_content: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricFilters {
    pub project_id: Option<ProjectId>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn grade_from_score_all_boundaries() {
        assert_eq!(Grade::from_score(100.0), Grade::A);
        assert_eq!(Grade::from_score(90.0), Grade::A);
        assert_eq!(Grade::from_score(89.9), Grade::B);
        assert_eq!(Grade::from_score(70.0), Grade::B);
        assert_eq!(Grade::from_score(69.9), Grade::C);
        assert_eq!(Grade::from_score(50.0), Grade::C);
        assert_eq!(Grade::from_score(49.9), Grade::D);
        assert_eq!(Grade::from_score(0.0), Grade::D);
    }

    #[test]
    fn grade_gc_interval_ordering() {
        assert!(Grade::A.recommended_gc_interval() > Grade::B.recommended_gc_interval());
        assert!(Grade::B.recommended_gc_interval() > Grade::C.recommended_gc_interval());
        assert!(Grade::C.recommended_gc_interval() > Grade::D.recommended_gc_interval());
    }

    #[test]
    fn thread_serialization_roundtrip() -> anyhow::Result<()> {
        let thread = Thread::new(PathBuf::from("/tmp/project"));
        let json = serde_json::to_string(&thread)?;
        let back: Thread = serde_json::from_str(&json)?;
        assert_eq!(thread.id, back.id);
        assert_eq!(thread.project_root, back.project_root);
        assert_eq!(thread.status, back.status);
        Ok(())
    }

    #[test]
    fn thread_status_serde_snake_case() -> anyhow::Result<()> {
        let s = serde_json::to_string(&ThreadStatus::Active)?;
        assert_eq!(s, "\"active\"");
        let back: ThreadStatus = serde_json::from_str("\"archived\"")?;
        assert_eq!(back, ThreadStatus::Archived);
        Ok(())
    }

    #[test]
    fn event_serialization_roundtrip() -> anyhow::Result<()> {
        let event = Event::new(SessionId::new(), "pre_tool_use", "Edit", Decision::Pass);
        let json = serde_json::to_string(&event)?;
        let back: Event = serde_json::from_str(&json)?;
        assert_eq!(event.id, back.id);
        assert_eq!(event.hook, back.hook);
        assert_eq!(event.decision, back.decision);
        Ok(())
    }

    #[test]
    fn signal_serialization_roundtrip() -> anyhow::Result<()> {
        let signal = Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({"count": 12}),
            RemediationType::Guard,
        );
        let json = serde_json::to_string(&signal)?;
        let back: Signal = serde_json::from_str(&json)?;
        assert_eq!(signal.id, back.id);
        assert_eq!(signal.signal_type, back.signal_type);
        Ok(())
    }

    #[test]
    fn item_user_message_roundtrip() -> anyhow::Result<()> {
        let item = Item::UserMessage {
            content: "hello world".to_string(),
        };
        let json = serde_json::to_string(&item)?;
        let back: Item = serde_json::from_str(&json)?;
        match back {
            Item::UserMessage { content } => assert_eq!(content, "hello world"),
            _ => anyhow::bail!("wrong variant"),
        }
        Ok(())
    }

    #[test]
    fn reasoning_budget_default_phase_mapping() {
        let budget = ReasoningBudget::default();
        // Planning and Validation use XHigh → opus
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Planning),
            "claude-opus-4-20250514"
        );
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Validation),
            "claude-opus-4-20250514"
        );
        // Execution uses High → sonnet
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Execution),
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn reasoning_budget_custom_models() {
        let budget = ReasoningBudget {
            planning_tier: BudgetTier::High,
            execution_tier: BudgetTier::Medium,
            validation_tier: BudgetTier::XHigh,
            xhigh_model: "opus-custom".to_string(),
            high_model: "sonnet-custom".to_string(),
            medium_model: "haiku-custom".to_string(),
        };
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Planning),
            "sonnet-custom"
        );
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Execution),
            "haiku-custom"
        );
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Validation),
            "opus-custom"
        );
    }

    #[test]
    fn reasoning_budget_serde_roundtrip() -> anyhow::Result<()> {
        let budget = ReasoningBudget::default();
        let json = serde_json::to_string(&budget)?;
        let back: ReasoningBudget = serde_json::from_str(&json)?;
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Planning),
            back.model_for_phase(ExecutionPhase::Planning)
        );
        assert_eq!(
            budget.model_for_phase(ExecutionPhase::Execution),
            back.model_for_phase(ExecutionPhase::Execution)
        );
        Ok(())
    }

    #[test]
    fn execution_phase_serde_snake_case() -> anyhow::Result<()> {
        assert_eq!(
            serde_json::to_string(&ExecutionPhase::Planning)?,
            "\"planning\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionPhase::Execution)?,
            "\"execution\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionPhase::Validation)?,
            "\"validation\""
        );
        let back: ExecutionPhase = serde_json::from_str("\"validation\"")?;
        assert_eq!(back, ExecutionPhase::Validation);
        Ok(())
    }
}
