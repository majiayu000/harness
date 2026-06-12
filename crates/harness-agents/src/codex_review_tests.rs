use super::*;
use std::fs;

fn review_request(base_ref: Option<&str>) -> CodexReviewRequest {
    CodexReviewRequest {
        project_root: PathBuf::from("/tmp/project"),
        instructions: Some("Return a harness-review-report block.".to_string()),
        base_ref: base_ref.map(str::to_string),
        model: Some("gpt-test".to_string()),
        reasoning_effort: Some("xhigh".to_string()),
        sandbox_mode: SandboxMode::ReadOnlyWithNetwork,
        approval_policy: Some("never".to_string()),
        env_vars: Default::default(),
    }
}

#[test]
fn review_args_use_native_review_subcommand_with_base_without_stdin_prompt() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = review_request(Some("origin/main"));

    let args: Vec<String> = agent
        .review_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(!args.iter().any(|arg| arg == "exec"));
    assert!(args
        .windows(2)
        .any(|window| window == ["-C", "/tmp/project"]));
    assert!(args.windows(2).any(|window| window == ["-m", "gpt-test"]));
    assert!(args
        .windows(2)
        .any(|window| window == ["-c", "model_reasoning_effort=\"xhigh\""]));
    assert!(args
        .windows(2)
        .any(|window| window == ["-c", "approval_policy=\"never\""]));
    assert!(args.windows(2).any(|window| {
        window[0] == "-c"
            && window[1]
                .starts_with("developer_instructions=\"Return a harness-review-report block.")
    }));
    assert!(args
        .windows(3)
        .any(|window| window == ["review", "--base", "origin/main"]));
    assert!(!args.iter().any(|arg| arg == "-"));
}

#[test]
fn review_args_use_stdin_prompt_without_base() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = review_request(None);

    let args: Vec<String> = agent
        .review_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(args.iter().any(|arg| arg == "review"));
    assert!(!args.iter().any(|arg| arg == "--base"));
    assert!(!args
        .iter()
        .any(|arg| arg.starts_with("developer_instructions=")));
    assert_eq!(args.last().map(String::as_str), Some("-"));
}

#[tokio::test]
async fn execute_review_omits_stdin_when_base_ref_is_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let args_file = dir.path().join("args.txt");
    let stdin_file = dir.path().join("stdin.txt");
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$@" > '{}'
cat > '{}'
printf '%s\n' '```harness-review-report'
printf '%s\n' '{{"decision":"approved","summary":"ok","findings":[]}}'
printf '%s\n' '```'
"#,
        args_file.display(),
        stdin_file.display(),
    );
    let script_path = dir.path().join("mock-codex-review.sh");
    fs::write(&script_path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms)?;
    }
    let agent = CodexAgent::new(script_path, SandboxMode::DangerFullAccess);

    let response = agent
        .execute_review(CodexReviewRequest {
            project_root: dir.path().to_path_buf(),
            instructions: Some("structured review instructions".to_string()),
            base_ref: Some("origin/main".to_string()),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("high".to_string()),
            sandbox_mode: SandboxMode::DangerFullAccess,
            approval_policy: Some("never".to_string()),
            env_vars: Default::default(),
        })
        .await?;

    let args = fs::read_to_string(args_file)?;
    assert!(args.lines().any(|line| line == "review"));
    assert!(args.lines().any(|line| line == "--base"));
    assert!(args.lines().any(|line| line == "origin/main"));
    assert!(args.lines().any(|line| {
        line.starts_with("developer_instructions=\"structured review instructions")
    }));
    assert!(!args.lines().any(|line| line == "-"));
    assert_eq!(fs::read_to_string(stdin_file)?, "");
    assert!(response.output.contains("```harness-review-report"));
    assert_eq!(response.exit_code, Some(0));
    Ok(())
}
