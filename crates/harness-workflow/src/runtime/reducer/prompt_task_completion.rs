use super::support::{runtime_blocked_command, runtime_completion_evidence};
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::prompt_task::{
    parse_external_state_signal, prompt_continuation_state_from_data, ExternalStateSignal,
    PromptContinuationState, PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
use chrono::Duration;
use serde_json::{json, Value};

pub(super) fn prompt_task_success_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    let continuation = match prompt_continuation_state_from_data(&instance.data) {
        Ok(Some(continuation)) => continuation,
        Ok(None) => return Some(single_shot_done_decision(instance, event, result)),
        Err(reason) => {
            return Some(blocked_decision(
                instance,
                event,
                result,
                "prompt_continuation_signal_missing",
                &reason,
                None,
            ));
        }
    };
    let signal = match parse_external_state_signal(result) {
        Ok(signal) => signal,
        Err(reason) => {
            return Some(blocked_decision(
                instance,
                event,
                result,
                "prompt_continuation_signal_missing",
                &reason,
                None,
            ));
        }
    };
    let observed = observed_state(&continuation, result, &signal);
    if !continuation.policy.active_states.contains(&signal.state) {
        return Some(settled_done_decision(
            instance, event, result, &signal, &observed,
        ));
    }
    if continuation.attempt >= continuation.policy.max_attempts {
        let reason = format!(
            "prompt continuation exhausted max_attempts={} while external state remained `{}`",
            continuation.policy.max_attempts, signal.state
        );
        return Some(blocked_decision(
            instance,
            event,
            result,
            "prompt_continuation_exhausted",
            &reason,
            Some((&signal, &observed)),
        ));
    }
    if observed.same_state_count >= continuation.policy.no_progress_limit {
        let reason = format!(
            "prompt continuation made no progress for {} consecutive attempts in external state `{}`",
            observed.same_state_count, signal.state
        );
        return Some(blocked_decision(
            instance,
            event,
            result,
            "prompt_continuation_no_progress",
            &reason,
            Some((&signal, &observed)),
        ));
    }
    Some(continue_decision(
        instance, event, result, &signal, observed,
    ))
}

fn single_shot_done_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "finish_prompt_task",
        "done",
        "prompt implementation activity completed successfully",
    )
    .with_command(mark_done_command(instance, result, None))
    .with_evidence(runtime_completion_evidence(event, result))
    .high_confidence()
}

fn settled_done_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    signal: &ExternalStateSignal,
    continuation: &PromptContinuationState,
) -> WorkflowDecision {
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "finish_prompt_task_external_settled",
        "done",
        format!(
            "external state `{}` is outside the configured active states",
            signal.state
        ),
    )
    .with_command(mark_done_command(instance, result, Some(continuation)))
    .with_evidence(runtime_completion_evidence(event, result))
    .with_evidence(signal.evidence())
    .high_confidence()
}

fn continue_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    signal: &ExternalStateSignal,
    mut continuation: PromptContinuationState,
) -> WorkflowDecision {
    continuation.attempt = continuation.attempt.saturating_add(1);
    let mut command = json!({
        "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "continuation": &continuation,
    });
    if continuation.policy.attempt_delay_secs > 0 {
        let delay = Duration::seconds(continuation.policy.attempt_delay_secs as i64);
        command["retry_not_before"] = json!((event.created_at + delay).to_rfc3339());
    }
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "continue_prompt_task",
        "implementing",
        format!(
            "external state `{}` remains active; enqueue attempt {}",
            signal.state, continuation.attempt
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "prompt-task:{}:attempt:{}",
            instance.id, continuation.attempt
        ),
        command,
    ))
    .with_evidence(runtime_completion_evidence(event, result))
    .with_evidence(signal.evidence())
    .high_confidence()
}

fn blocked_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    decision_id: &str,
    reason: &str,
    signal_and_state: Option<(&ExternalStateSignal, &PromptContinuationState)>,
) -> WorkflowDecision {
    let mut block = runtime_blocked_command(
        reason,
        None,
        format!("runtime-completion:{}:{decision_id}:block", event.id),
        event,
        result,
    );
    if let Some((_, continuation)) = signal_and_state {
        block.command["continuation"] = json!(continuation);
    }
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_id,
        "blocked",
        reason,
    )
    .with_command(block)
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!("runtime-completion:{}:{decision_id}:operator", event.id),
        json!({
            "reason": reason,
            "activity": result.activity,
        }),
    ))
    .with_evidence(runtime_completion_evidence(event, result));
    decision = match signal_and_state {
        Some((signal, _)) => decision.with_evidence(signal.evidence()),
        None => decision.with_evidence(WorkflowEvidence::new(
            "external_state",
            external_state_evidence_summary(result),
        )),
    };
    decision.high_confidence()
}

fn observed_state(
    previous: &PromptContinuationState,
    result: &ActivityResult,
    signal: &ExternalStateSignal,
) -> PromptContinuationState {
    let no_progress = previous.last_external_state.as_deref() == Some(signal.state.as_str())
        && result.artifacts.is_empty()
        && result.validation.is_empty();
    PromptContinuationState {
        policy: previous.policy.clone(),
        attempt: previous.attempt,
        last_external_state: Some(signal.state.clone()),
        last_summary: Some(result.summary.clone()),
        same_state_count: if no_progress {
            previous.same_state_count.saturating_add(1)
        } else {
            0
        },
    }
}

fn mark_done_command(
    instance: &WorkflowInstance,
    result: &ActivityResult,
    continuation: Option<&PromptContinuationState>,
) -> WorkflowCommand {
    let mut payload = json!({
        "activity": result.activity,
        "workflow_id": instance.id,
    });
    if let Some(continuation) = continuation {
        payload["continuation"] = json!(continuation);
    }
    WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        format!("prompt-task:{}:done", instance.id),
        payload,
    )
}

fn external_state_evidence_summary(result: &ActivityResult) -> String {
    let values = result
        .signals
        .iter()
        .filter(|signal| signal.signal_type == "external_state")
        .map(|signal| signal.signal.clone())
        .collect::<Vec<Value>>();
    json!({ "signals": values }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{ActivitySignal, ValidationRecord, WorkflowSubject};
    use crate::runtime::prompt_task::PromptContinuationPolicy;
    use std::collections::BTreeSet;

    fn policy(max_attempts: u32, no_progress_limit: u32) -> PromptContinuationPolicy {
        PromptContinuationPolicy {
            max_attempts,
            attempt_delay_secs: 30,
            active_states: BTreeSet::from(["In Progress".to_string()]),
            no_progress_limit,
        }
    }

    fn instance(continuation: Option<PromptContinuationState>) -> WorkflowInstance {
        let mut data = json!({});
        if let Some(continuation) = continuation {
            data["continuation"] = json!(continuation);
        }
        WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-1"),
        )
        .with_id("workflow-1")
        .with_data(data)
    }

    fn event(result: &ActivityResult) -> WorkflowEvent {
        WorkflowEvent::new("workflow-1", 1, "RuntimeJobCompleted", "runtime-1").with_payload(
            json!({
                "command_id": "command-1",
                "runtime_job_id": "job-1",
                "activity_result": result,
            }),
        )
    }

    fn result(state: Option<&str>) -> ActivityResult {
        let result = ActivityResult::succeeded(PROMPT_TASK_IMPLEMENT_ACTIVITY, "attempt summary");
        match state {
            Some(state) => result.with_signal(ActivitySignal::new(
                "external_state",
                json!({ "state": state, "subject": "TEAM-123" }),
            )),
            None => result,
        }
    }

    #[test]
    fn prompt_continuation_preserves_single_shot_and_settled_done_paths() {
        let validated = result(None).with_validation(ValidationRecord::new("cargo test", "passed"));
        let decision =
            prompt_task_success_decision(&instance(None), &event(&validated), &validated)
                .expect("single shot decision");
        assert_eq!(decision.decision, "finish_prompt_task");
        assert_eq!(decision.next_state, "done");

        let continuation = PromptContinuationState::initial(&policy(4, 3));
        let settled =
            result(Some("Done")).with_validation(ValidationRecord::new("cargo test", "passed"));
        let decision =
            prompt_task_success_decision(&instance(Some(continuation)), &event(&settled), &settled)
                .expect("settled decision");
        assert_eq!(decision.decision, "finish_prompt_task_external_settled");
        assert_eq!(decision.next_state, "done");
        assert!(decision.evidence.iter().any(|e| e.kind == "external_state"));
    }

    #[test]
    fn prompt_continuation_active_state_enqueues_next_attempt_with_context_and_delay() {
        let continuation = PromptContinuationState::initial(&policy(4, 3));
        let active = result(Some("In Progress"));
        let completion = event(&active);
        let expected_not_before = (completion.created_at + Duration::seconds(30)).to_rfc3339();
        let decision =
            prompt_task_success_decision(&instance(Some(continuation)), &completion, &active)
                .expect("continue decision");
        assert_eq!(decision.decision, "continue_prompt_task");
        assert_eq!(decision.next_state, "implementing");
        assert_eq!(decision.commands.len(), 1);
        assert_eq!(
            decision.commands[0].dedupe_key,
            "prompt-task:workflow-1:attempt:2"
        );
        assert_eq!(decision.commands[0].command["continuation"]["attempt"], 2);
        assert_eq!(
            decision.commands[0].command["continuation"]["last_summary"],
            "attempt summary"
        );
        assert_eq!(
            decision.commands[0].command["retry_not_before"],
            expected_not_before
        );
    }

    #[test]
    fn prompt_continuation_blocks_malformed_exhausted_and_no_progress_results() {
        let malformed = result(None);
        let malformed_decision = prompt_task_success_decision(
            &instance(Some(PromptContinuationState::initial(&policy(4, 3)))),
            &event(&malformed),
            &malformed,
        )
        .expect("malformed decision");
        assert_eq!(
            malformed_decision.decision,
            "prompt_continuation_signal_missing"
        );
        assert_eq!(malformed_decision.next_state, "blocked");

        let exhausted = PromptContinuationState {
            attempt: 2,
            ..PromptContinuationState::initial(&policy(2, 3))
        };
        let active = result(Some("In Progress"));
        let exhausted_decision =
            prompt_task_success_decision(&instance(Some(exhausted)), &event(&active), &active)
                .expect("exhausted decision");
        assert_eq!(exhausted_decision.decision, "prompt_continuation_exhausted");

        let stalled = PromptContinuationState {
            attempt: 2,
            last_external_state: Some("In Progress".to_string()),
            same_state_count: 1,
            ..PromptContinuationState::initial(&policy(4, 2))
        };
        let stalled_decision =
            prompt_task_success_decision(&instance(Some(stalled)), &event(&active), &active)
                .expect("stalled decision");
        assert_eq!(stalled_decision.decision, "prompt_continuation_no_progress");
        assert_eq!(
            stalled_decision.commands[0].command["continuation"]["same_state_count"],
            2
        );
    }
}
