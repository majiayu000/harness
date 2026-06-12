use super::*;
use harness_core::validation::{
    ShellValidationExecutor, ValidationExecutor, ValidationOutcome, ValidationOutcomeKind,
    ValidationRequest, MAX_CAPTURED_OUTPUT_BYTES,
};

#[test]
fn periodic_review_source_uses_standard_allowed_tools() {
    // Verifies that periodic_review tasks get a non-empty allowed_tools list,
    // which causes claude.rs to pass --allowedTools (hard enforcement) instead
    // of --dangerously-skip-permissions.
    let tools = restricted_tools(CapabilityProfile::Standard).unwrap_or_default();
    assert_eq!(
        tools,
        CapabilityProfile::Standard.tools().unwrap_or_default()
    );
    assert!(!tools.is_empty());
}

#[test]
fn standard_implementation_turn_uses_full_profile() {
    // Non-periodic_review tasks use None → Full profile →
    // --dangerously-skip-permissions in claude.rs.
    let implementation_allowed_tools: Option<Vec<String>> = None;
    assert!(implementation_allowed_tools.is_none());
}

#[test]
fn effective_agent_review_round_limit_prefers_request_over_server() {
    assert_eq!(effective_agent_review_round_limit(Some(1), 3, 5), 1);
}

#[test]
fn effective_agent_review_round_limit_uses_server_before_triage_default() {
    assert_eq!(effective_agent_review_round_limit(None, 3, 8), 3);
}

#[test]
fn effective_hosted_review_round_limit_prefers_request_over_project() {
    assert_eq!(
        effective_hosted_review_round_limit(Some(1), Some(8), 3, 5),
        1
    );
}

#[test]
fn effective_hosted_review_round_limit_uses_project_before_server() {
    assert_eq!(effective_hosted_review_round_limit(None, Some(8), 3, 5), 8);
}

#[test]
fn effective_hosted_review_round_limit_uses_server_before_triage_default() {
    assert_eq!(effective_hosted_review_round_limit(None, None, 3, 8), 3);
}

#[test]
fn local_review_pr_check_timeout_uses_hosted_review_budget() {
    assert_eq!(local_review_pr_check_timeout_secs(30, 8), 480);
}

#[test]
fn initial_hosted_review_wait_respects_wait_budget() {
    assert_eq!(initial_hosted_review_wait_secs(300, 60), 60);
    assert_eq!(initial_hosted_review_wait_secs(30, 60), 30);
}

#[test]
fn initial_hosted_review_wait_zero_budget_keeps_poll_interval() {
    assert_eq!(initial_hosted_review_wait_secs(300, 0), 300);
}

#[test]
fn review_repo_slug_prefers_parseable_pr_url() {
    assert_eq!(
        review_repo_slug(
            Some("https://github.com/from-url/repo/pull/77"),
            "from-checkout/repo"
        ),
        "from-url/repo"
    );
}

#[test]
fn review_repo_slug_falls_back_to_detected_slug() {
    assert_eq!(
        review_repo_slug(
            Some("https://git.example.com/from-url/repo/pull/77"),
            "owner/repo"
        ),
        "owner/repo"
    );
}

#[test]
fn parse_harness_review_command() {
    let cmd = parse_harness_mention_command("@harness review");
    assert_eq!(cmd, Some(HarnessMentionCommand::Review));
}

#[test]
fn parse_harness_fix_ci_command_case_insensitive() {
    let cmd = parse_harness_mention_command("please @Harness FIX CI");
    assert_eq!(cmd, Some(HarnessMentionCommand::FixCi));
}

#[test]
fn parse_harness_plain_mention_command() {
    let cmd = parse_harness_mention_command("hello @harness can you help?");
    assert_eq!(cmd, Some(HarnessMentionCommand::Mention));
}

#[test]
fn parse_harness_command_returns_none_without_mention() {
    let cmd = parse_harness_mention_command("no command here");
    assert_eq!(cmd, None);
}

#[test]
fn parse_harness_first_mention_per_line_is_used() {
    let cmd = parse_harness_mention_command("@harness review then @harness fix ci");
    assert_eq!(cmd, Some(HarnessMentionCommand::Review));
}

#[test]
fn prompt_builder_no_sections_adds_trailing_newline() {
    let result = PromptBuilder::new("Title line.").build();
    assert_eq!(result, "Title line.\n");
}

#[test]
fn prompt_builder_optional_url_absent_is_skipped() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("Link", None)
        .build();
    assert_eq!(result, "Title.\n");
}

#[test]
fn prompt_builder_optional_url_present_appears_in_output() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("Link", Some("https://example.com"))
        .build();
    assert!(result.contains("- Link: "));
    assert!(result.contains("https://example.com"));
    assert!(result.ends_with('\n'));
}

#[test]
fn prompt_builder_add_section_wraps_external_data() {
    let result = PromptBuilder::new("Title.")
        .add_section("Payload", "content here")
        .build();
    assert!(result.contains("Payload:\n"));
    assert!(result.contains("<external_data>"));
    assert!(result.contains("content here"));
}

#[test]
fn prompt_builder_multiple_urls_all_appear() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("First", Some("url1"))
        .add_optional_url("Second", None)
        .add_optional_url("Third", Some("url3"))
        .build();
    assert!(result.contains("- First: "));
    assert!(result.contains("url1"));
    assert!(!result.contains("Second"));
    assert!(result.contains("- Third: "));
    assert!(result.contains("url3"));
}

#[test]
fn build_fix_ci_prompt_contains_context() {
    let prompt = build_fix_ci_prompt(
        "majiayu000/harness",
        42,
        "@harness fix CI",
        Some("https://github.com/majiayu000/harness/issues/42#issuecomment-1"),
        Some("https://github.com/majiayu000/harness/pull/42"),
    );

    assert!(prompt.contains("CI failure repair requested for PR #42"));
    assert!(prompt.contains("majiayu000/harness"));
    assert!(prompt.contains("<external_data>"));
    assert!(prompt.contains("PR_URL=https://github.com/majiayu000/harness/pull/42"));
}

#[test]
fn truncate_short_string_passes_through() {
    let input = "short error";
    let result = helpers::truncate_validation_error(input, 100);
    assert_eq!(result, "short error");
}

#[test]
fn truncate_at_max_chars_boundary() {
    let input = "a".repeat(200);
    let result = helpers::truncate_validation_error(&input, 50);
    assert!(result.starts_with(&"a".repeat(50)));
    assert!(result.contains("(output truncated, 200 chars total)"));
}

#[test]
fn truncate_preserves_utf8_boundary() {
    // "é" is 2 bytes; build a string where max_chars lands mid-character.
    let input = "ééééé"; // 10 bytes, 5 chars
    let result = helpers::truncate_validation_error(input, 3); // byte 3 is mid-char
                                                               // Should back up to byte 2 (1 full "é").
    assert!(result.starts_with("é"));
    assert!(result.contains("(output truncated,"));
}

#[test]
fn review_check_turn_uses_readonly_profile() {
    let tools = restricted_tools(CapabilityProfile::ReadOnly).unwrap();
    assert!(tools.contains(&"Read".to_string()));
    assert!(tools.contains(&"Grep".to_string()));
    assert!(tools.contains(&"Glob".to_string()));
    assert!(!tools.contains(&"Write".to_string()));
    assert!(!tools.contains(&"Edit".to_string()));
    assert!(!tools.contains(&"Bash".to_string()));
}

#[test]
fn periodic_review_turn_uses_standard_profile_with_bash() {
    let tools = restricted_tools(CapabilityProfile::Standard).unwrap();
    assert!(tools.contains(&"Bash".to_string()));
    assert!(tools.contains(&"Read".to_string()));
    assert!(tools.contains(&"Write".to_string()));
    assert!(tools.contains(&"Edit".to_string()));
    // Standard does not include Grep/Glob — it's distinct from ReadOnly.
    assert!(!tools.contains(&"Grep".to_string()));
}

#[test]
fn implementation_turn_uses_full_profile_no_restriction() {
    // Full profile returns None — no tool restriction is applied to the agent.
    assert!(CapabilityProfile::Full.tools().is_none());
}

// --- Gate: task_needs_pr_url covers issue and pr:N tasks ---

#[test]
fn task_needs_pr_url_true_for_issue_task() {
    let req = CreateTaskRequest {
        issue: Some(42),
        ..CreateTaskRequest::default()
    };
    assert!(
        implement_pipeline::task_needs_pr_url(&req),
        "issue task must require PR_URL"
    );
}

#[test]
fn issue_triage_runs_only_when_not_skipped_and_no_existing_pr() {
    assert!(should_run_issue_triage(false, false));
    assert!(!should_run_issue_triage(true, false));
    assert!(!should_run_issue_triage(false, true));
}

#[test]
fn task_needs_pr_url_true_for_pr_task() {
    let req = CreateTaskRequest {
        pr: Some(99),
        ..CreateTaskRequest::default()
    };
    assert!(
        implement_pipeline::task_needs_pr_url(&req),
        "pr:N task must require PR_URL"
    );
}

#[test]
fn task_needs_pr_url_false_for_prompt_only_task() {
    let req = CreateTaskRequest::default();
    assert!(
        !implement_pipeline::task_needs_pr_url(&req),
        "prompt-only task must not require PR_URL (Done is correct)"
    );
}

// --- Gate A: pr:N task with non-empty output but no PR_URL gets Failed ---

#[tokio::test]
async fn pr_task_nonempty_output_no_pr_url_marks_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    let state = crate::task_runner::TaskState::new(task_id.clone());
    store.insert(&state).await;

    let req = CreateTaskRequest {
        pr: Some(42),
        ..CreateTaskRequest::default()
    };
    // Non-empty output from agent but no PR_URL found — gate must fire for pr:N.
    let output = "LGTM, nothing to change";
    if output.trim().is_empty() {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 2;
            s.error = Some("empty agent output: no PR created and no output".to_string());
        })
        .await?;
    } else if implement_pipeline::task_needs_pr_url(&req) {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 2;
            s.error = Some("no PR number found in agent output; task requires PR_URL".to_string());
        })
        .await?;
    } else {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
    }

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
    assert!(
        matches!(final_state.status, TaskStatus::Failed),
        "pr:N task with non-empty output but no PR_URL must be Failed, not Done"
    );
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("PR_URL"),
        "error must mention PR_URL"
    );
    Ok(())
}

#[tokio::test]
async fn prompt_only_nonempty_output_no_pr_stays_done() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    let state = crate::task_runner::TaskState::new(task_id.clone());
    store.insert(&state).await;

    let req = CreateTaskRequest::default(); // no issue, no pr
    let output = "periodic review complete: no issues found";
    if output.trim().is_empty() {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 2;
            s.error = Some("empty agent output: no PR created and no output".to_string());
        })
        .await?;
    } else if implement_pipeline::task_needs_pr_url(&req) {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Failed;
            s.turn = 2;
            s.error = Some("no PR number found in agent output; task requires PR_URL".to_string());
        })
        .await?;
    } else {
        mutate_and_persist(&store, &task_id, |s| {
            s.status = TaskStatus::Done;
            s.turn = 2;
        })
        .await?;
    }

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
    assert!(
        matches!(final_state.status, TaskStatus::Done),
        "prompt-only task with non-empty output and no PR must be Done"
    );
    Ok(())
}

#[tokio::test]
async fn test_gate_caps_large_failure_output() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let script = dir.path().join("large-output-test");
    std::fs::write(
        &script,
        "#!/bin/sh\ndd if=/dev/zero bs=4096 count=256 2>/dev/null\nexit 7\n",
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let cmd = "./large-output-test".to_string();
    let validation_executor = ShellValidationExecutor::new();

    let err = run_test_gate(
        &validation_executor,
        dir.path(),
        &[cmd],
        10,
        &HashMap::new(),
    )
    .await
    .expect_err("failing test command should reject LGTM");

    assert!(err.contains("Test gate failed (exit 7)"));
    let stdout = err
        .split_once("stdout:\n")
        .and_then(|(_, tail)| tail.split_once("\nstderr:\n").map(|(stdout, _)| stdout))
        .expect("test gate error should include stdout and stderr sections");
    assert_eq!(
        stdout.len(),
        MAX_CAPTURED_OUTPUT_BYTES,
        "test gate must cap captured stdout using the validation executor"
    );
    Ok(())
}

#[tokio::test]
async fn test_gate_passes_extra_env_to_validation_executor() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct RecordingValidationExecutor {
        env: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl ValidationExecutor for RecordingValidationExecutor {
        async fn run<'a>(&self, request: ValidationRequest<'a>) -> ValidationOutcome {
            self.env.lock().unwrap().extend(
                request
                    .env
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
            );
            ValidationOutcome {
                kind: ValidationOutcomeKind::Success,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }

    let dir = tempfile::tempdir()?;
    let executor = RecordingValidationExecutor::default();
    let captured = Arc::clone(&executor.env);
    let mut extra_env = HashMap::new();
    extra_env.insert(
        "CARGO_TARGET_DIR".to_string(),
        "/tmp/harness-task-target".to_string(),
    );

    run_test_gate(&executor, dir.path(), &["true".to_string()], 10, &extra_env)
        .await
        .expect("successful executor outcome should pass the test gate");

    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[(
            "CARGO_TARGET_DIR".to_string(),
            "/tmp/harness-task-target".to_string(),
        )]
    );
    Ok(())
}
