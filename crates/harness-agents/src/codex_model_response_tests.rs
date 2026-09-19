use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn model_echo_agent(root: &Path) -> anyhow::Result<CodexAgent> {
    let cli_path = root.join("codex-model-echo");
    // Echo the actual -m argument, so the assertions cover the launch and
    // response together rather than only testing model-selection helpers.
    std::fs::write(
        &cli_path,
        r#"#!/bin/sh
set -eu
model=
mode=exec
while [ "$#" -gt 0 ]; do
    case "$1" in
        -m) model="$2"; shift 2 ;;
        review) mode=review; shift ;;
        *) shift ;;
    esac
done
if [ "$mode" = review ]; then
    printf '%s\n' "$model"
else
    printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$model"
    printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}\n'
fi
"#,
    )?;
    std::fs::set_permissions(&cli_path, std::fs::Permissions::from_mode(0o755))?;
    let mut agent = CodexAgent::new(cli_path, SandboxMode::DangerFullAccess);
    agent.default_model = "configured-model".to_string();
    Ok(agent.with_stream_timeout(Some(5)))
}

#[tokio::test]
async fn execute_response_reports_launch_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let agent = model_echo_agent(dir.path())?;
    for (requested, expected) in [
        (None, "configured-model"),
        (Some("request-model-a"), "request-model-a"),
        (Some("request-model-b"), "request-model-b"),
    ] {
        let request = AgentRequest {
            prompt: "report selected model".to_string(),
            prompt_layers: None,
            project_root: dir.path().to_path_buf(),
            permission_mode: AgentPermissionMode::Full,
            model: requested.map(str::to_string),
            reasoning_effort: None,
            execution_phase: None,
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            approval_policy: None,
            allowed_tools: None,
            max_budget_usd: None,
            context: Vec::new(),
            timeout_secs: Some(5),
            env_vars: HashMap::new(),
            capability_token: None,
        };
        let response = tokio::time::timeout(Duration::from_secs(10), agent.execute(request))
            .await??;
        assert_eq!(response.model, expected);
        assert_eq!(response.output.trim(), expected);
        assert_eq!(response.exit_code, Some(0));
        assert_eq!(agent.name(), "codex");
    }
    Ok(())
}

#[tokio::test]
async fn review_response_reports_launch_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join(".git"))?;
    let agent = model_echo_agent(dir.path())?;
    for (requested, expected) in [
        (None, "configured-model"),
        (Some("request-model-a"), "request-model-a"),
        (Some("request-model-b"), "request-model-b"),
    ] {
        let request = CodexReviewRequest {
            project_root: dir.path().to_path_buf(),
            instructions: None,
            base_ref: None,
            model: requested.map(str::to_string),
            reasoning_effort: None,
            sandbox_mode: SandboxMode::DangerFullAccess,
            approval_policy: None,
            permission_mode: AgentPermissionMode::Full,
            env_vars: HashMap::new(),
        };
        let response = tokio::time::timeout(Duration::from_secs(10), agent.execute_review(request))
            .await??;
        assert_eq!(response.model, expected);
        assert_eq!(response.output.trim(), expected);
        assert_eq!(response.exit_code, Some(0));
        assert_eq!(agent.name(), "codex");
    }
    Ok(())
}
