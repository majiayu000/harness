use super::*;

#[test]
fn runtime_submission_completion_task_preserves_issue_intake_identity() {
    let instance = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "cancelled",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({
        "task_id": "runtime-task-42",
        "project_id": "/tmp/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "source": "github",
        "external_id": "issue:42",
    }));
    let result = ActivityResult::cancelled("implement_issue", "Runtime job was cancelled.");

    let task = runtime_submission_completion_task(&instance, Some(&result))
        .expect("terminal runtime issue should map to an intake task");

    assert_eq!(task.id.as_str(), "runtime-task-42");
    assert_eq!(task.task_kind, TaskKind::Issue);
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(task.source.as_deref(), Some("github"));
    assert_eq!(task.external_id.as_deref(), Some("issue:42"));
    assert_eq!(task.repo.as_deref(), Some("owner/repo"));
    assert_eq!(task.issue, Some(42));
}

#[test]
fn runtime_submission_completion_task_preserves_prompt_intake_identity() {
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "done",
        WorkflowSubject::new("prompt", "manual:prompt:42"),
    )
    .with_data(json!({
        "task_id": "runtime-prompt-42",
        "project_id": "/tmp/project",
        "prompt_summary": "prompt task",
        "source": "dashboard",
        "external_id": "manual:prompt:42",
    }));
    let result = ActivityResult::succeeded("implement_prompt", "Prompt task completed.");

    let task = runtime_submission_completion_task(&instance, Some(&result))
        .expect("terminal runtime prompt should map to an intake task");

    assert_eq!(task.id.as_str(), "runtime-prompt-42");
    assert_eq!(task.task_kind, TaskKind::Prompt);
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.source.as_deref(), Some("dashboard"));
    assert_eq!(task.external_id.as_deref(), Some("manual:prompt:42"));
    assert_eq!(task.issue, None);
    assert_eq!(task.description.as_deref(), Some("prompt task"));
}

#[test]
fn runtime_submission_completion_task_marks_retryable_failures_as_transient() {
    let instance = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "failed",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_data(json!({
        "task_id": "runtime-task-42",
        "project_id": "/tmp/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "source": "github",
        "external_id": "issue:42",
    }));
    let result = ActivityResult::failed(
        "implement_issue",
        "Runtime dependency failed.",
        "provider temporarily unavailable",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency);

    let task = runtime_submission_completion_task(&instance, Some(&result))
        .expect("terminal runtime issue should map to an intake task");

    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.failure_kind, Some(TaskFailureKind::WorkspaceLifecycle));
    assert_eq!(
        task.error.as_deref(),
        Some("provider temporarily unavailable")
    );
}

#[test]
fn runtime_profile_approval_policy_accepts_codex_values() {
    for value in ["untrusted", "on-failure", "on-request", "never"] {
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
        profile.approval_policy = Some(value.to_string());

        assert_eq!(
            runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
                .expect("codex approval policy should be accepted"),
            Some(value.to_string())
        );
    }
}

#[test]
fn runtime_profile_approval_policy_rejects_unknown_values() {
    let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexExec);
    profile.approval_policy = Some("always".to_string());

    let error = runtime_profile_approval_policy(&profile, RuntimeKind::CodexExec)
        .expect_err("unknown approval policy should fail");

    assert!(error
        .to_string()
        .contains("runtime profile approval_policy `always` is not supported"));
}

#[test]
fn runtime_profile_approval_policy_rejects_non_codex_runtimes() {
    let mut profile = RuntimeProfile::new("claude-default", RuntimeKind::ClaudeCode);
    profile.approval_policy = Some("on-request".to_string());

    let error = runtime_profile_approval_policy(&profile, RuntimeKind::ClaudeCode)
        .expect_err("Claude approval policy should fail until it has a contract");

    assert!(error
        .to_string()
        .contains("only supported for Codex runtime kinds"));
}
