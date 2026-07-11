use super::*;

#[test]
fn codex_sandbox_mode_maps_to_codex_cli_values() {
    assert_eq!(codex_sandbox_mode(SandboxMode::ReadOnly), "read-only");
    assert_eq!(
        codex_sandbox_mode(SandboxMode::ReadOnlyWithNetwork),
        "read-only"
    );
    assert_eq!(
        codex_sandbox_mode(SandboxMode::WorkspaceWrite),
        "workspace-write"
    );
    assert_eq!(
        codex_sandbox_mode(SandboxMode::DangerFullAccess),
        "danger-full-access"
    );
}

#[test]
fn base_args_uses_request_reasoning_effort_sandbox_and_approval_policy() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        reasoning_effort: Some("medium".to_string()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        approval_policy: Some("on-request".to_string()),
        ..Default::default()
    };

    let args: Vec<String> = agent
        .base_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(args
        .windows(2)
        .any(|window| window == ["-c", "model_reasoning_effort=\"medium\""]));
    assert!(args.windows(2).any(|window| window == ["-s", "read-only"]));
    assert!(args
        .windows(2)
        .any(|window| window == ["-c", "approval_policy=\"on-request\""]));
    assert!(!args.iter().any(|arg| arg == "-a"));
    assert_eq!(args.last().map(String::as_str), Some("ping"));
}

#[test]
fn base_args_configures_read_only_with_network_profile() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        sandbox_mode: Some(SandboxMode::ReadOnlyWithNetwork),
        ..Default::default()
    };

    let args: Vec<String> = agent
        .base_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();

    assert!(!args.iter().any(|arg| arg == "-s"));
    assert!(args.windows(2).any(|window| {
        window
            == [
                "-c",
                "default_permissions=\"harness_read_only_with_network\"",
            ]
    }));
    assert!(args.windows(2).any(|window| {
        window == [
            "-c",
            "permissions.harness_read_only_with_network.filesystem={\":minimal\"=\"read\",\":project_roots\"={\".\"=\"read\"}}",
        ]
    }));
    assert!(args.windows(2).any(|window| {
        window
            == [
                "-c",
                "permissions.harness_read_only_with_network.network.enabled=true",
            ]
    }));
}

#[test]
fn spawn_diagnostics_resolve_relative_program_from_spawn_current_dir() {
    let project = match tempdir() {
        Ok(project) => project,
        Err(error) => panic!("create project dir: {error}"),
    };
    let bin_dir = project.path().join("bin");
    if let Err(error) = fs::create_dir(&bin_dir) {
        panic!("create bin dir: {error}");
    }
    let program_path = bin_dir.join("codex");
    if let Err(error) = fs::write(&program_path, "#!/bin/sh\n") {
        panic!("write program: {error}");
    }

    let resolved = match resolve_program_for_spawn(Path::new("bin/codex"), project.path()) {
        Some(resolved) => resolved,
        None => panic!("relative program should resolve from spawn current dir"),
    };

    assert_eq!(resolved, program_path);
}

#[test]
fn allowed_tools_does_not_override_configured_workspace_write_sandbox() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::WorkspaceWrite);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
        ..Default::default()
    };

    let args: Vec<String> = agent
        .base_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();
    assert!(args
        .windows(2)
        .any(|window| window == ["-s", "workspace-write"]));
}

#[test]
fn deny_all_allowed_tools_keeps_configured_sandbox_mode() {
    let agent = CodexAgent::new(PathBuf::from("codex"), SandboxMode::DangerFullAccess);
    let request = AgentRequest {
        prompt: "ping".to_string(),
        project_root: PathBuf::from("/tmp/project"),
        allowed_tools: Some(vec![]),
        ..Default::default()
    };

    let args: Vec<String> = agent
        .base_args(&request)
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect();
    assert!(args
        .windows(2)
        .any(|window| window == ["-s", "danger-full-access"]));
}
