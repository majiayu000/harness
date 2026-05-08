use super::*;

#[test]
fn activity_name_uses_top_level_runtime_activity_key() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "start_child_workflow",
            "command_type": "start_child_workflow",
            "command": {
                "definition_id": "github_issue_pr",
                "subject_key": "issue:123"
            }
        }),
    );

    assert_eq!(activity_name(&job), "start_child_workflow");
}

#[test]
fn activity_name_falls_back_to_command_type() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "command_type": "start_child_workflow",
            "command": {
                "definition_id": "github_issue_pr",
                "subject_key": "issue:123"
            }
        }),
    );

    assert_eq!(activity_name(&job), "start_child_workflow");
}

#[test]
fn prompt_task_request_blocks_when_cached_payload_is_unavailable() -> anyhow::Result<()> {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "command": {
                "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "prompt_ref": "prompt-submission:cache-miss-test"
            }
        }),
    );

    let request = prompt_task_request_for_job(&job)?;
    assert_eq!(
        request,
        PromptTaskRequest::PayloadUnavailable {
            prompt_ref: "prompt-submission:cache-miss-test".to_string()
        }
    );

    let result = prompt_payload_unavailable_result(&job, "prompt-submission:cache-miss-test");
    assert_eq!(result.status, ActivityStatus::Blocked);
    assert_eq!(result.activity, PROMPT_TASK_IMPLEMENT_ACTIVITY);
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("only held in the current Harness process"));
    assert_eq!(
        result.artifacts[0].artifact_type,
        "prompt_payload_unavailable"
    );
    Ok(())
}

#[test]
fn dependency_task_ids_from_command_maps_issue_numbers_to_repo_handles() {
    let command = json!({
        "depends_on": [42, "43", "explicit-task-id"]
    });

    let dependencies = dependency_task_ids_from_command(&command, Some("owner/repo"));

    assert_eq!(
        dependencies
            .iter()
            .map(|task_id| task_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "repo-backlog:owner/repo:issue:42",
            "repo-backlog:owner/repo:issue:43",
            "explicit-task-id"
        ]
    );
}

#[test]
fn stable_runtime_workspace_task_id_reuses_workflow_identity_across_jobs() {
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:124"),
    )
    .with_id("/repo/root::repo:owner/repo::issue:124");
    let first_job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    );
    let second_job = RuntimeJob::pending(
        "command-2",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "inspect_pr_feedback" }),
    );

    let first = stable_runtime_workspace_task_id(&first_job, Some(&workflow));
    let second = stable_runtime_workspace_task_id(&second_job, Some(&workflow));

    assert_eq!(first, second);
    assert!(first.as_str().starts_with("runtime-wf-github-issue-pr-"));
    assert!(first
        .as_str()
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
}

#[test]
fn runtime_workspace_finish_action_preserves_reusable_issue_workspaces() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    );
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:124"),
    )
    .with_id("issue-124");

    assert_eq!(
        runtime_workspace_finish_action("on_terminal", true, &job, Some(&workflow)),
        RuntimeWorkspaceFinishAction::Release
    );
}

#[test]
fn runtime_workspace_finish_action_removes_ephemeral_or_non_reused_workspaces() {
    let backlog_job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": REPO_BACKLOG_POLL_ACTIVITY }),
    );
    let backlog = WorkflowInstance::new(
        REPO_BACKLOG_DEFINITION_ID,
        1,
        "scanning",
        WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-owner-repo");
    assert_eq!(
        runtime_workspace_finish_action("on_terminal", true, &backlog_job, Some(&backlog)),
        RuntimeWorkspaceFinishAction::Remove
    );

    let issue_job = RuntimeJob::pending(
        "command-2",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    );
    let issue = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:124"),
    )
    .with_id("issue-124");
    assert_eq!(
        runtime_workspace_finish_action("on_terminal", false, &issue_job, Some(&issue)),
        RuntimeWorkspaceFinishAction::Remove
    );
}

#[test]
fn activity_result_schema_describes_quality_gate_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": QUALITY_GATE_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", "issue:123"),
    )
    .with_id("quality-gate-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], QUALITY_GATE_DEFINITION_ID);
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
        "passed"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["output_signal"],
        QUALITY_PASSED_SIGNAL
    );
    assert_eq!(
        schema["agent_summary_contract"]["must_include"][0],
        "validation commands"
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "passed"));
}

#[test]
fn activity_result_schema_describes_repo_backlog_poll_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "poll_repo_backlog"
        }),
    );
    let workflow = WorkflowInstance::new(
        "repo_backlog",
        1,
        "scanning",
        WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], "repo_backlog");
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "IssueDiscovered"
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["IssueDiscovered"],
        "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available."
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "planning_batch"));
}

#[test]
fn activity_result_schema_describes_repo_sprint_plan_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": harness_workflow::runtime::REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        "repo_backlog",
        1,
        "planning_batch",
        WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], "repo_backlog");
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "SprintTaskSelected"
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["SprintTaskSelected"],
        "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers."
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "dispatching"));
}

#[test]
fn activity_result_schema_describes_pr_feedback_child_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], PR_FEEDBACK_DEFINITION_ID);
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "FeedbackFound"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["parent_propagation"],
        "The same activity result is propagated to the parent github_issue_pr workflow."
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["PrReadyToMerge"],
        "Use only when review, checks, and mergeability are all ready."
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "feedback_found"));
}

#[test]
fn runtime_prompt_packet_includes_workflow_file_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "runtime_profile": RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc)
        }),
    );
    let workflow_document = WorkflowDocument {
        prompt_template: "Follow the repository workflow prompt.".to_string(),
        source_path: Some("/repo/WORKFLOW.md".to_string()),
        ..Default::default()
    };
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    let packet = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &workflow_document,
    );
    assert_eq!(packet["project"]["root"], "/workspaces/job-1");
    assert_eq!(packet["project"]["source_root"], "/repo");
    assert_eq!(
        packet["workflow_file"]["prompt_template"],
        "Follow the repository workflow prompt."
    );

    let prompt = build_runtime_job_prompt(&packet, None);
    assert!(prompt.contains("Repository workflow prompt template:"));
    assert!(prompt.contains("Follow the repository workflow prompt."));
}

#[test]
fn activity_result_from_turn_fails_when_no_fenced_block_present() {
    // P0-1: a completed agent turn that emits no `harness-activity-result`
    // fenced block must NOT be silently treated as success. Returning
    // succeeded here historically caused state-machine no-progress loops
    // for `repo_backlog` workflows (claude returns prose, reducer falls
    // back to `finish_repo_backlog_scan`, state cycles back to idle, next
    // tick re-dispatches, ad infinitum).
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::ClaudeCode,
        "claude-default",
        json!({
            "activity": "poll_repo_backlog"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: "I scanned the repo and saw no new issues. Done.".to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "claude",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "poll_repo_backlog");
    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Failed,
        "missing structured result MUST surface as failed"
    );
    assert_eq!(
        result.error_kind,
        Some(harness_workflow::runtime::ActivityErrorKind::Configuration),
        "missing structured result is a configuration-class failure (prompt or agent contract issue)"
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|e| e.contains("harness-activity-result")),
        "error message MUST mention the missing block so operators can diagnose"
    );
}

#[test]
fn activity_result_from_turn_parses_structured_activity_result_block() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"Implementation completed.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    assert_eq!(result.summary, "Implementation completed.");
    let pr_artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "pull_request")
        .expect("structured pull request artifact should be preserved");
    assert_eq!(pr_artifact.artifact["pr_number"], 77);
    assert_eq!(
        pr_artifact.artifact["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    let prompt_artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_prompt_packet")
        .expect("runtime prompt artifact should be appended");
    assert_eq!(prompt_artifact.artifact["digest"], "digest-1");
    let turn_artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_turn")
        .expect("runtime turn artifact should be appended");
    assert_eq!(turn_artifact.artifact["thread_id"], "thread-1");
    assert_eq!(turn_artifact.artifact["turn_id"], "turn-1");
    assert!(result
        .signals
        .iter()
        .any(|signal| signal.signal_type == "RuntimeTurnCompleted"));
}

#[test]
fn activity_result_from_turn_fails_mismatched_structured_activity() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let content = r#"Wrong activity result.

```harness-activity-result
{"activity":"replan_issue","status":"succeeded","summary":"Wrong activity.","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":77,"pr_url":"https://github.com/owner/repo/pull/77"}}]}
```"#;
    let items = vec![Item::AgentReasoning {
        content: content.to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert_eq!(result.summary, "Structured activity result was invalid.");
    assert_eq!(
        result.error.as_deref(),
        Some("activity result block reported activity `replan_issue`, expected `implement_issue`")
    );
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_turn"));
}

#[test]
fn activity_result_from_turn_fails_latest_malformed_structured_activity() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::AgentReasoning {
        content: r#"Work completed.

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":"stale summary","artifacts":[{"artifact_type":"pull_request","artifact":{"pr_number":66,"pr_url":"https://github.com/owner/repo/pull/66"}}]}
```

Final result:

```harness-activity-result
{"activity":"implement_issue","status":"succeeded","summary":
```"#
            .to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Completed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert_eq!(result.summary, "Structured activity result was invalid.");
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with("activity result block is invalid JSON:")));
    assert!(!result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "pull_request"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_prompt_packet"));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "runtime_turn"));
}

#[test]
fn activity_result_from_turn_classifies_timeout_error_kind() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let items = vec![Item::Error {
        code: 1,
        message: "Agent turn timed out after 30s".to_string(),
    }];

    let result = activity_result_from_turn(
        &job,
        &TurnStatus::Failed,
        &items,
        &ThreadId::from_str("thread-1"),
        &TurnId::from_str("turn-1"),
        "codex",
        Path::new("/project"),
        "digest-1",
    );

    assert_eq!(result.activity, "implement_issue");
    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert_eq!(result.error_kind, Some(ActivityErrorKind::Timeout));
    assert_eq!(
        result.error.as_deref(),
        Some("Agent turn timed out after 30s")
    );
}
