use super::*;

#[test]
fn validate_request_enforces_prompt_continuation_contract() {
    let valid_policy = harness_workflow::runtime::PromptContinuationPolicy {
        max_attempts: 4,
        attempt_delay_secs: 30,
        active_states: std::collections::BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    };
    let valid = CreateTaskRequest {
        prompt: Some("Continue TEAM-123".to_string()),
        continuation: Some(valid_policy.clone()),
        ..Default::default()
    };
    assert!(DefaultExecutionService::validate_request(&valid).is_ok());

    let wrong_kind = CreateTaskRequest {
        issue: Some(123),
        continuation: Some(valid_policy.clone()),
        ..Default::default()
    };
    assert!(matches!(
        DefaultExecutionService::validate_request(&wrong_kind),
        Err(EnqueueTaskError::BadRequest(message))
            if message == "continuation is only supported for prompt-only tasks"
    ));

    let invalid = CreateTaskRequest {
        prompt: Some("Continue TEAM-123".to_string()),
        continuation: Some(harness_workflow::runtime::PromptContinuationPolicy {
            max_attempts: 0,
            ..valid_policy
        }),
        ..Default::default()
    };
    assert!(matches!(
        DefaultExecutionService::validate_request(&invalid),
        Err(EnqueueTaskError::BadRequest(message))
            if message.contains("max_attempts must be at least 1")
    ));
}
