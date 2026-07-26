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
fn review_args_use_spawn_working_directory_with_base_without_stdin_prompt() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = review_request(Some("origin/main"));

    let args: Vec<String> = agent
        .review_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(!args.iter().any(|arg| arg == "exec"));
    assert!(!args.iter().any(|arg| arg == "-C"));
    assert!(!args.iter().any(|arg| arg == "/tmp/project"));
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

fn container_review_request(
    root: &std::path::Path,
    base_ref: Option<&str>,
    env_vars: Vec<(&str, &str)>,
) -> CodexReviewRequest {
    let mut env: std::collections::HashMap<String, String> = env_vars
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    env.insert(
        "HARNESS_AGENT_ISOLATION_TIER".to_string(),
        "container".to_string(),
    );
    CodexReviewRequest {
        project_root: root.to_path_buf(),
        instructions: Some("structured review instructions".to_string()),
        base_ref: base_ref.map(str::to_string),
        model: Some("gpt-test".to_string()),
        reasoning_effort: Some("high".to_string()),
        sandbox_mode: SandboxMode::WorkspaceWrite,
        approval_policy: Some("never".to_string()),
        env_vars: env,
    }
}

fn spawn_args(spawn: &crate::spawn_contract::PreparedAgentSpawn) -> Vec<String> {
    spawn
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// GH-1785: `execute_review` used to call `wrap_command` directly, so review
/// runs got neither container isolation nor operator-secret env filtering.
#[test]
fn review_spawn_uses_container_isolation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);

    let (spawn, _) = agent.prepare_review_spawn(&container_review_request(
        dir.path(),
        Some("origin/main"),
        vec![],
    ))?;

    let args = spawn_args(&spawn);
    assert_eq!(spawn.program, PathBuf::from("docker"));
    assert!(spawn.clear_inherited_env);
    assert_eq!(spawn.current_dir, std::fs::canonicalize(dir.path())?);
    assert!(args.contains(&"--mount".to_string()));
    assert!(args
        .windows(2)
        .any(|window| window == ["--workdir", "/workspace"]));
    assert!(!args.iter().any(|arg| arg == "-C"));
    assert!(args.contains(&"review".to_string()));
    Ok(())
}

#[test]
fn review_spawn_filters_operator_secrets() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);

    let (spawn, _) = agent.prepare_review_spawn(&container_review_request(
        dir.path(),
        Some("origin/main"),
        vec![
            ("GITHUB_TOKEN", "operator-token"),
            ("ANTHROPIC_API_KEY", "operator-key"),
            ("HARNESS_SCOPED_GITHUB_TOKEN", "scoped-token"),
        ],
    ))?;

    let args = spawn_args(&spawn);
    assert!(!args.iter().any(|arg| arg.contains("operator-token")));
    assert!(!args.iter().any(|arg| arg.contains("operator-key")));
    assert!(!spawn.process_env.contains_key("GITHUB_TOKEN"));
    assert!(!spawn.process_env.contains_key("ANTHROPIC_API_KEY"));
    // The scoped token is the only credential the container receives.
    assert!(args.contains(&"GITHUB_TOKEN=scoped-token".to_string()));
    assert!(!args
        .iter()
        .any(|arg| arg.starts_with("HARNESS_SCOPED_GITHUB_TOKEN=")));
    Ok(())
}

/// A piped prompt is lost unless the container keeps stdin open.
#[test]
fn review_spawn_keeps_container_stdin_open_for_piped_prompt() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);

    let (piped, _) =
        agent.prepare_review_spawn(&container_review_request(dir.path(), None, vec![]))?;
    let (not_piped, _) = agent.prepare_review_spawn(&container_review_request(
        dir.path(),
        Some("origin/main"),
        vec![],
    ))?;

    assert!(spawn_args(&piped).contains(&"--interactive".to_string()));
    assert!(!spawn_args(&not_piped).contains(&"--interactive".to_string()));
    Ok(())
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
