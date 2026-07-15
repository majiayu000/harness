use super::model::{
    ActivityResult, WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;

pub const PROMPT_TASK_DEFINITION_ID: &str = "prompt_task";
pub const PROMPT_TASK_IMPLEMENT_ACTIVITY: &str = "implement_prompt";
pub const PROMPT_CONTINUATION_MAX_ATTEMPTS: u32 = 20;
pub const PROMPT_CONTINUATION_MAX_DELAY_SECS: u64 = 3_600;
pub const PROMPT_CONTINUATION_DEFAULT_NO_PROGRESS_LIMIT: u32 = 3;

fn default_no_progress_limit() -> u32 {
    PROMPT_CONTINUATION_DEFAULT_NO_PROGRESS_LIMIT
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptContinuationPolicy {
    pub max_attempts: u32,
    pub attempt_delay_secs: u64,
    pub active_states: BTreeSet<String>,
    #[serde(default = "default_no_progress_limit")]
    pub no_progress_limit: u32,
}

impl PromptContinuationPolicy {
    pub fn validate(&self) -> Result<(), PromptContinuationPolicyError> {
        if self.max_attempts == 0 {
            return Err(PromptContinuationPolicyError::new(
                "continuation.max_attempts must be at least 1",
            ));
        }
        if self.max_attempts > PROMPT_CONTINUATION_MAX_ATTEMPTS {
            return Err(PromptContinuationPolicyError::new(format!(
                "continuation.max_attempts must not exceed {PROMPT_CONTINUATION_MAX_ATTEMPTS}"
            )));
        }
        if self.attempt_delay_secs > PROMPT_CONTINUATION_MAX_DELAY_SECS {
            return Err(PromptContinuationPolicyError::new(format!(
                "continuation.attempt_delay_secs must not exceed {PROMPT_CONTINUATION_MAX_DELAY_SECS}"
            )));
        }
        if self.active_states.is_empty() {
            return Err(PromptContinuationPolicyError::new(
                "continuation.active_states must contain at least one state",
            ));
        }
        if self
            .active_states
            .iter()
            .any(|state| state.trim().is_empty())
        {
            return Err(PromptContinuationPolicyError::new(
                "continuation.active_states must not contain an empty state",
            ));
        }
        if self.no_progress_limit == 0 {
            return Err(PromptContinuationPolicyError::new(
                "continuation.no_progress_limit must be at least 1",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptContinuationPolicyError {
    message: String,
}

impl PromptContinuationPolicyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PromptContinuationPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PromptContinuationPolicyError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptContinuationState {
    pub policy: PromptContinuationPolicy,
    pub attempt: u32,
    pub last_external_state: Option<String>,
    pub last_summary: Option<String>,
    pub same_state_count: u32,
    #[serde(default)]
    pub last_progress_fingerprint: Option<String>,
}

impl PromptContinuationState {
    pub fn initial(policy: &PromptContinuationPolicy) -> Self {
        Self {
            policy: policy.clone(),
            attempt: 1,
            last_external_state: None,
            last_summary: None,
            same_state_count: 0,
            last_progress_fingerprint: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalStateSignal {
    pub state: String,
    pub payload: Value,
}

impl ExternalStateSignal {
    pub fn evidence(&self) -> WorkflowEvidence {
        WorkflowEvidence::new("external_state", self.payload.to_string())
    }
}

pub fn prompt_continuation_state_from_data(
    data: &Value,
) -> Result<Option<PromptContinuationState>, String> {
    let Some(value) = data.get("continuation") else {
        return Ok(None);
    };
    let state: PromptContinuationState = serde_json::from_value(value.clone())
        .map_err(|error| format!("persisted continuation state is invalid: {error}"))?;
    state
        .policy
        .validate()
        .map_err(|error| format!("persisted continuation policy is invalid: {error}"))?;
    if state.attempt == 0 || state.attempt > state.policy.max_attempts {
        return Err(format!(
            "persisted continuation attempt {} is outside 1..={}",
            state.attempt, state.policy.max_attempts
        ));
    }
    Ok(Some(state))
}

pub fn parse_external_state_signal(result: &ActivityResult) -> Result<ExternalStateSignal, String> {
    let mut signals = result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == "external_state");
    let Some(signal) = signals.next() else {
        return Err(
            "implement_prompt result must contain exactly one external_state signal".into(),
        );
    };
    if signals.next().is_some() {
        return Err("implement_prompt result contains multiple external_state signals".into());
    }
    let Some(payload) = signal.signal.as_object() else {
        return Err("external_state signal payload must be a JSON object".into());
    };
    let Some(state) = payload.get("state").and_then(Value::as_str) else {
        return Err("external_state signal payload must contain a string state field".into());
    };
    if state.trim().is_empty() {
        return Err("external_state signal state must not be empty".into());
    }
    Ok(ExternalStateSignal {
        state: state.to_string(),
        payload: signal.signal.clone(),
    })
}

pub fn continuation_value(policy: &PromptContinuationPolicy) -> Value {
    json!({
        "policy": policy,
        "attempt": 1,
        "last_external_state": null,
        "last_summary": null,
        "same_state_count": 0,
        "last_progress_fingerprint": null,
    })
}

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
    pub continuation: Option<&'a PromptContinuationPolicy>,
}

#[derive(Debug, Clone)]
pub struct PromptSubmissionDecisionOutput {
    pub action: PromptTaskWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_prompt_submission_decision(
    instance: &WorkflowInstance,
    input: PromptSubmissionDecisionInput<'_>,
) -> Result<PromptSubmissionDecisionOutput, PromptContinuationPolicyError> {
    if let Some(policy) = input.continuation {
        policy.validate()?;
    }
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
        let dedupe_key = if input.continuation.is_some() {
            format!("prompt-task:{}:attempt:1", instance.id)
        } else {
            format!(
                "prompt-submit:{}:task:{}:implement",
                subject_key(input),
                input.task_id
            )
        };
        decision = decision.with_command(WorkflowCommand::new(
            super::model::WorkflowCommandType::EnqueueActivity,
            dedupe_key,
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

    Ok(PromptSubmissionDecisionOutput {
        action: PromptTaskWorkflowAction::RunImplementation,
        decision,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> PromptContinuationPolicy {
        PromptContinuationPolicy {
            max_attempts: 4,
            attempt_delay_secs: 30,
            active_states: BTreeSet::from(["In Progress".to_string()]),
            no_progress_limit: 3,
        }
    }

    #[test]
    fn prompt_continuation_policy_enforces_bounds() {
        assert!(policy().validate().is_ok());
        for invalid in [
            PromptContinuationPolicy {
                max_attempts: 0,
                ..policy()
            },
            PromptContinuationPolicy {
                max_attempts: PROMPT_CONTINUATION_MAX_ATTEMPTS + 1,
                ..policy()
            },
            PromptContinuationPolicy {
                attempt_delay_secs: PROMPT_CONTINUATION_MAX_DELAY_SECS + 1,
                ..policy()
            },
            PromptContinuationPolicy {
                active_states: BTreeSet::new(),
                ..policy()
            },
            PromptContinuationPolicy {
                no_progress_limit: 0,
                ..policy()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn external_state_signal_parser_is_strict() {
        let valid = ActivityResult::succeeded(PROMPT_TASK_IMPLEMENT_ACTIVITY, "active")
            .with_signal(super::super::model::ActivitySignal::new(
                "external_state",
                json!({ "state": "In Progress", "subject": "TEAM-123" }),
            ));
        assert_eq!(
            parse_external_state_signal(&valid)
                .expect("valid signal")
                .state,
            "In Progress"
        );

        let non_object =
            ActivityResult::succeeded(PROMPT_TASK_IMPLEMENT_ACTIVITY, "bad").with_signal(
                super::super::model::ActivitySignal::new("external_state", json!("In Progress")),
            );
        assert!(parse_external_state_signal(&non_object).is_err());
        let ambiguous = valid
            .clone()
            .with_signal(super::super::model::ActivitySignal::new(
                "external_state",
                json!({ "state": "Done" }),
            ));
        assert!(parse_external_state_signal(&ambiguous).is_err());
    }

    #[test]
    fn prompt_continuation_state_accepts_legacy_data_without_progress_fingerprint() {
        let state: PromptContinuationState = serde_json::from_value(json!({
            "policy": policy(),
            "attempt": 2,
            "last_external_state": "In Progress",
            "last_summary": "legacy attempt",
            "same_state_count": 1
        }))
        .expect("legacy continuation state should remain readable");

        assert_eq!(state.attempt, 2);
        assert_eq!(state.last_progress_fingerprint, None);
    }
}
