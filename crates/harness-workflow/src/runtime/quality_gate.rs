use super::model::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvidence, WorkflowInstance,
};
use serde_json::json;

pub const QUALITY_GATE_DEFINITION_ID: &str = "quality_gate";
pub const QUALITY_GATE_ACTIVITY: &str = "run_quality_gate";
pub const QUALITY_PASSED_SIGNAL: &str = "QualityPassed";
pub const QUALITY_FAILED_SIGNAL: &str = "QualityFailed";
pub const QUALITY_BLOCKED_SIGNAL: &str = "QualityBlocked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityGateWorkflowAction {
    RunQualityGate,
}

#[derive(Debug, Clone, Copy)]
pub struct QualityGateDecisionInput<'a> {
    pub reason: &'a str,
    pub validation_commands: &'a [String],
}

#[derive(Debug, Clone)]
pub struct QualityGateDecisionOutput {
    pub action: QualityGateWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn quality_gate_workflow_id(project_id: &str, subject_key: &str) -> String {
    format!("{project_id}::quality_gate::{subject_key}")
}

pub fn build_quality_gate_run_decision(
    instance: &WorkflowInstance,
    input: QualityGateDecisionInput<'_>,
) -> QualityGateDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "run_quality_gate",
        "checking",
        input.reason,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!("quality-gate:{}:run", instance.subject.subject_key),
        json!({
            "activity": QUALITY_GATE_ACTIVITY,
            "validation_commands": input.validation_commands,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "quality_gate",
        format!("validation_commands={}", input.validation_commands.len()),
    ))
    .high_confidence();

    QualityGateDecisionOutput {
        action: QualityGateWorkflowAction::RunQualityGate,
        decision,
    }
}
