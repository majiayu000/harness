use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowDefinition {
    pub id: String,
    pub version: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub definition_hash: String,
    pub active: bool,
}

impl WorkflowDefinition {
    pub fn new(id: impl Into<String>, version: u32, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version,
            name: name.into(),
            source_path: None,
            definition_hash: String::new(),
            active: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSubject {
    pub subject_type: String,
    pub subject_key: String,
}

impl WorkflowSubject {
    pub fn new(subject_type: impl Into<String>, subject_key: impl Into<String>) -> Self {
        Self {
            subject_type: subject_type.into(),
            subject_key: subject_key.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowLease {
    pub owner: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInstance {
    pub id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub state: String,
    pub subject: WorkflowSubject,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_workflow_id: Option<String>,
    #[serde(default)]
    pub data: Value,
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<WorkflowLease>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorkflowInstance {
    pub fn new(
        definition_id: impl Into<String>,
        definition_version: u32,
        state: impl Into<String>,
        subject: WorkflowSubject,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            definition_id: definition_id.into(),
            definition_version,
            state: state.into(),
            subject,
            parent_workflow_id: None,
            data: Value::Object(Default::default()),
            version: 0,
            lease: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    pub fn with_parent(mut self, parent_workflow_id: impl Into<String>) -> Self {
        self.parent_workflow_id = Some(parent_workflow_id.into());
        self
    }

    pub fn with_lease(mut self, owner: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        self.lease = Some(WorkflowLease {
            owner: owner.into(),
            expires_at,
        });
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowEvent {
    pub id: String,
    pub workflow_id: String,
    pub sequence: u64,
    pub event_type: String,
    #[serde(default)]
    pub event: Value,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl WorkflowEvent {
    pub fn new(
        workflow_id: impl Into<String>,
        sequence: u64,
        event_type: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_id: workflow_id.into(),
            sequence,
            event_type: event_type.into(),
            event: Value::Object(Default::default()),
            source: source.into(),
            causation_id: None,
            correlation_id: None,
            created_at: Utc::now(),
        }
    }

    pub fn with_payload(mut self, event: Value) -> Self {
        self.event = event;
        self
    }

    pub fn with_correlation(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCommandType {
    EnqueueActivity,
    StartChildWorkflow,
    BindPr,
    RecordPlanConcern,
    Wait,
    MarkBlocked,
    MarkDone,
    MarkFailed,
    MarkCancelled,
    RequestOperatorAttention,
}

impl WorkflowCommandType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EnqueueActivity => "enqueue_activity",
            Self::StartChildWorkflow => "start_child_workflow",
            Self::BindPr => "bind_pr",
            Self::RecordPlanConcern => "record_plan_concern",
            Self::Wait => "wait",
            Self::MarkBlocked => "mark_blocked",
            Self::MarkDone => "mark_done",
            Self::MarkFailed => "mark_failed",
            Self::MarkCancelled => "mark_cancelled",
            Self::RequestOperatorAttention => "request_operator_attention",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowCommand {
    pub command_type: WorkflowCommandType,
    pub dedupe_key: String,
    #[serde(default)]
    pub command: Value,
}

impl WorkflowCommand {
    pub fn new(
        command_type: WorkflowCommandType,
        dedupe_key: impl Into<String>,
        command: Value,
    ) -> Self {
        Self {
            command_type,
            dedupe_key: dedupe_key.into(),
            command,
        }
    }

    pub fn enqueue_activity(activity: impl Into<String>, dedupe_key: impl Into<String>) -> Self {
        Self::new(
            WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            json!({ "activity": activity.into() }),
        )
    }

    pub fn start_child_workflow(
        definition_id: impl Into<String>,
        subject_key: impl Into<String>,
        dedupe_key: impl Into<String>,
    ) -> Self {
        Self::new(
            WorkflowCommandType::StartChildWorkflow,
            dedupe_key,
            json!({
                "definition_id": definition_id.into(),
                "subject_key": subject_key.into()
            }),
        )
    }

    pub fn bind_pr(
        pr_number: u64,
        pr_url: impl Into<String>,
        dedupe_key: impl Into<String>,
    ) -> Self {
        Self::new(
            WorkflowCommandType::BindPr,
            dedupe_key,
            json!({
                "pr_number": pr_number,
                "pr_url": pr_url.into()
            }),
        )
    }

    pub fn wait(reason: impl Into<String>, dedupe_key: impl Into<String>) -> Self {
        Self::new(
            WorkflowCommandType::Wait,
            dedupe_key,
            json!({ "reason": reason.into() }),
        )
    }

    pub fn mark_blocked(reason: impl Into<String>, dedupe_key: impl Into<String>) -> Self {
        Self::new(
            WorkflowCommandType::MarkBlocked,
            dedupe_key,
            json!({ "reason": reason.into() }),
        )
    }

    pub fn record_plan_concern(concern: impl Into<String>, dedupe_key: impl Into<String>) -> Self {
        Self::new(
            WorkflowCommandType::RecordPlanConcern,
            dedupe_key,
            json!({ "concern": concern.into() }),
        )
    }

    pub fn requires_runtime_job(&self) -> bool {
        matches!(
            self.command_type,
            WorkflowCommandType::EnqueueActivity | WorkflowCommandType::StartChildWorkflow
        )
    }

    pub fn activity_name(&self) -> Option<&str> {
        self.command
            .get("activity")
            .and_then(|value| value.as_str())
    }

    pub fn runtime_activity_key(&self) -> &str {
        self.activity_name()
            .unwrap_or_else(|| self.command_type.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowCommandRecord {
    pub id: String,
    pub workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub status: String,
    pub command: WorkflowCommand,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionConfidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowEvidence {
    pub kind: String,
    pub summary: String,
}

impl WorkflowEvidence {
    pub fn new(kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDecision {
    pub workflow_id: String,
    pub observed_state: String,
    pub decision: String,
    pub next_state: String,
    pub reason: String,
    pub confidence: DecisionConfidence,
    #[serde(default)]
    pub commands: Vec<WorkflowCommand>,
    #[serde(default)]
    pub evidence: Vec<WorkflowEvidence>,
}

impl WorkflowDecision {
    pub fn new(
        workflow_id: impl Into<String>,
        observed_state: impl Into<String>,
        decision: impl Into<String>,
        next_state: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            observed_state: observed_state.into(),
            decision: decision.into(),
            next_state: next_state.into(),
            reason: reason.into(),
            confidence: DecisionConfidence::Medium,
            commands: Vec::new(),
            evidence: Vec::new(),
        }
    }

    pub fn with_command(mut self, command: WorkflowCommand) -> Self {
        self.commands.push(command);
        self
    }

    pub fn with_evidence(mut self, evidence: WorkflowEvidence) -> Self {
        self.evidence.push(evidence);
        self
    }

    pub fn high_confidence(mut self) -> Self {
        self.confidence = DecisionConfidence::High;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDecisionRecord {
    pub id: String,
    pub workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub decision: WorkflowDecision,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl WorkflowDecisionRecord {
    pub fn accepted(decision: WorkflowDecision, event_id: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_id: decision.workflow_id.clone(),
            event_id,
            decision,
            accepted: true,
            rejection_reason: None,
            created_at: Utc::now(),
        }
    }

    pub fn rejected(
        decision: WorkflowDecision,
        event_id: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            workflow_id: decision.workflow_id.clone(),
            event_id,
            decision,
            accepted: false,
            rejection_reason: Some(reason.into()),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    CodexExec,
    CodexJsonrpc,
    ClaudeCode,
    AnthropicApi,
    RemoteHost,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeProfile {
    pub name: String,
    pub kind: RuntimeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl RuntimeProfile {
    pub fn new(name: impl Into<String>, kind: RuntimeKind) -> Self {
        Self {
            name: name.into(),
            kind,
            model: None,
            reasoning_effort: None,
            sandbox: None,
            approval_policy: None,
            max_turns: None,
            timeout_secs: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeJobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeJob {
    pub id: String,
    pub command_id: String,
    pub runtime_kind: RuntimeKind,
    pub runtime_profile: String,
    pub status: RuntimeJobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<WorkflowLease>,
    #[serde(default)]
    pub input: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RuntimeJob {
    pub fn pending(
        command_id: impl Into<String>,
        runtime_kind: RuntimeKind,
        runtime_profile: impl Into<String>,
        input: Value,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            command_id: command_id.into(),
            runtime_kind,
            runtime_profile: runtime_profile.into(),
            status: RuntimeJobStatus::Pending,
            lease: None,
            input,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn claim(&mut self, owner: impl Into<String>, expires_at: DateTime<Utc>) {
        self.status = RuntimeJobStatus::Running;
        self.lease = Some(WorkflowLease {
            owner: owner.into(),
            expires_at,
        });
        self.updated_at = Utc::now();
    }

    pub fn complete(&mut self, result: &ActivityResult) -> anyhow::Result<()> {
        self.status = match result.status {
            ActivityStatus::Succeeded => RuntimeJobStatus::Succeeded,
            ActivityStatus::Failed => RuntimeJobStatus::Failed,
            ActivityStatus::Blocked => RuntimeJobStatus::Failed,
            ActivityStatus::Cancelled => RuntimeJobStatus::Cancelled,
        };
        self.output = Some(serde_json::to_value(result)?);
        self.error = result.error.clone();
        self.lease = None;
        self.updated_at = Utc::now();
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeEvent {
    pub id: String,
    pub runtime_job_id: String,
    pub sequence: u64,
    pub event_type: String,
    #[serde(default)]
    pub event: Value,
    pub created_at: DateTime<Utc>,
}

impl RuntimeEvent {
    pub fn new(
        runtime_job_id: impl Into<String>,
        sequence: u64,
        event_type: impl Into<String>,
        event: Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            runtime_job_id: runtime_job_id.into(),
            sequence,
            event_type: event_type.into(),
            event,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityArtifact {
    pub artifact_type: String,
    #[serde(default)]
    pub artifact: Value,
}

impl ActivityArtifact {
    pub fn new(artifact_type: impl Into<String>, artifact: Value) -> Self {
        Self {
            artifact_type: artifact_type.into(),
            artifact,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivitySignal {
    pub signal_type: String,
    #[serde(default)]
    pub signal: Value,
}

impl ActivitySignal {
    pub fn new(signal_type: impl Into<String>, signal: Value) -> Self {
        Self {
            signal_type: signal_type.into(),
            signal,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationRecord {
    pub command: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ValidationRecord {
    pub fn new(command: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            status: status.into(),
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityResult {
    pub activity: String,
    pub status: ActivityStatus,
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ActivityArtifact>,
    #[serde(default)]
    pub signals: Vec<ActivitySignal>,
    #[serde(default)]
    pub validation: Vec<ValidationRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ActivityResult {
    pub fn succeeded(activity: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            activity: activity.into(),
            status: ActivityStatus::Succeeded,
            summary: summary.into(),
            artifacts: Vec::new(),
            signals: Vec::new(),
            validation: Vec::new(),
            error: None,
        }
    }

    pub fn failed(
        activity: impl Into<String>,
        summary: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            activity: activity.into(),
            status: ActivityStatus::Failed,
            summary: summary.into(),
            artifacts: Vec::new(),
            signals: Vec::new(),
            validation: Vec::new(),
            error: Some(error.into()),
        }
    }

    pub fn cancelled(activity: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            activity: activity.into(),
            status: ActivityStatus::Cancelled,
            summary: summary.into(),
            artifacts: Vec::new(),
            signals: Vec::new(),
            validation: Vec::new(),
            error: None,
        }
    }

    pub fn with_artifact(mut self, artifact: ActivityArtifact) -> Self {
        self.artifacts.push(artifact);
        self
    }

    pub fn with_signal(mut self, signal: ActivitySignal) -> Self {
        self.signals.push(signal);
        self
    }

    pub fn with_validation(mut self, validation: ValidationRecord) -> Self {
        self.validation.push(validation);
        self
    }
}
