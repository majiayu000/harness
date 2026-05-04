use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};

pub const PROMPT_TASK_DEFINITION_ID: &str = "prompt_task";
pub const PROMPT_TASK_IMPLEMENT_ACTIVITY: &str = "implement_prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptTaskWorkflowAction {
    RunImplementation,
}

#[derive(Debug, Clone, Copy)]
pub struct PromptSubmissionDecisionInput<'a> {
    pub task_id: &'a str,
    pub prompt: &'a str,
    pub prompt_ref: &'a str,
    pub source: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub depends_on: &'a [String],
    pub dependencies_blocked: bool,
}

#[derive(Debug, Clone)]
pub struct PromptSubmissionDecisionOutput {
    pub action: PromptTaskWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_prompt_submission_decision(
    instance: &WorkflowInstance,
    input: PromptSubmissionDecisionInput<'_>,
) -> PromptSubmissionDecisionOutput {
    let next_state = if input.dependencies_blocked {
        "awaiting_dependencies"
    } else {
        "implementing"
    };
    let reason = if input.dependencies_blocked {
        "operator submitted the prompt task and it is waiting for dependencies"
    } else {
        "operator submitted the prompt task for implementation"
    };
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "submit_prompt",
        next_state,
        reason,
    );
    if !input.dependencies_blocked {
        decision = decision.with_command(WorkflowCommand::new(
            super::model::WorkflowCommandType::EnqueueActivity,
            format!(
                "prompt-submit:{}:task:{}:implement",
                subject_key(input),
                input.task_id
            ),
            serde_json::json!({
                "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "prompt_ref": input.prompt_ref,
                "prompt_chars": input.prompt.chars().count(),
                "source": input.source,
                "external_id": input.external_id,
                "task_id": input.task_id,
            }),
        ));
    }
    let decision = decision
        .with_evidence(WorkflowEvidence::new(
            "prompt_submission",
            format!(
                "task_id={} prompt_chars={} source={} external_id={} depends_on={} dependencies_blocked={}",
                input.task_id,
                input.prompt.chars().count(),
                input.source.unwrap_or("<none>"),
                input.external_id.unwrap_or("<none>"),
                depends_on_summary(input.depends_on),
                input.dependencies_blocked
            ),
        ))
        .high_confidence();

    PromptSubmissionDecisionOutput {
        action: PromptTaskWorkflowAction::RunImplementation,
        decision,
    }
}

fn subject_key(input: PromptSubmissionDecisionInput<'_>) -> &str {
    input.external_id.unwrap_or(input.task_id)
}

fn depends_on_summary(depends_on: &[String]) -> String {
    if depends_on.is_empty() {
        return "<none>".to_string();
    }
    depends_on.join(",")
}
