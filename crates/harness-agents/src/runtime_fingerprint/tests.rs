use super::*;
use harness_core::stack::{AgentStackSource, AgentStackSourceScope};

fn source(name: &str) -> ConfiguredRuntimeSource {
    ConfiguredRuntimeSource::without_canonical_bytes(
        AgentStackSource::logical(AgentStackSourceScope::System, "runtime", name).unwrap(),
    )
    .unwrap()
}

fn sandbox(mode: SandboxMode) -> SandboxSpec {
    SandboxSpec::new(mode, "/definitely/not/observed")
}

fn configured(isolation: IsolationTier, sandbox: SandboxSpec) -> ConfiguredRuntimeExecutable {
    ConfiguredRuntimeExecutable::new(
        LocalExecutableRuntimeKind::CodexExec,
        source("codex"),
        isolation,
        sandbox,
        "codex",
        Vec::new(),
    )
}

#[test]
fn local_executable_runtime_kind_is_closed_and_uses_fixed_args_and_output_grammars() {
    assert_eq!(LocalExecutableRuntimeKind::ALL.len(), 3);
    for kind in LocalExecutableRuntimeKind::ALL {
        assert_eq!(kind.version_args(), ["--version"]);
    }
}

#[tokio::test]
async fn container_isolation_fails_before_host_resolution() {
    let error = fingerprint_configured_runtime_executable(
        &configured(
            IsolationTier::Container,
            sandbox(SandboxMode::DangerFullAccess),
        ),
        &RuntimeFingerprintOptions::new("/missing"),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintError::UnsupportedIsolation(IsolationTier::Container)
    ));
}

#[tokio::test]
async fn microvm_isolation_fails_before_host_resolution() {
    let error = fingerprint_configured_runtime_executable(
        &configured(
            IsolationTier::Microvm,
            sandbox(SandboxMode::DangerFullAccess),
        ),
        &RuntimeFingerprintOptions::new("/missing"),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintError::UnsupportedIsolation(IsolationTier::Microvm)
    ));
}

#[tokio::test]
async fn restricted_sandbox_fails_before_host_observation() {
    let error = fingerprint_configured_runtime_executable(
        &configured(IsolationTier::Host, sandbox(SandboxMode::ReadOnly)),
        &RuntimeFingerprintOptions::new("/missing"),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintError::SandboxParityUnavailable
    ));
}

#[tokio::test]
async fn narrowed_allowed_write_paths_fail_before_host_observation() {
    let narrowed = sandbox(SandboxMode::DangerFullAccess)
        .with_allowed_write_paths(vec![PathBuf::from("/tmp/allowed")]);
    let error = fingerprint_configured_runtime_executable(
        &configured(IsolationTier::Host, narrowed),
        &RuntimeFingerprintOptions::new("/missing"),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintError::SandboxParityUnavailable
    ));
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn unsupported_platform_fails_before_output_or_cwd_validation() {
    let error = fingerprint_configured_runtime_executable(
        &configured(IsolationTier::Host, sandbox(SandboxMode::DangerFullAccess)),
        &RuntimeFingerprintOptions::new("/missing").with_max_output_bytes(0),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintError::ContainmentUnavailable(
            ContainmentUnavailableReason::UnsupportedPlatform
        )
    ));
}

#[test]
fn runtime_fingerprint_maps_agents_config_to_explicit_sources() {
    let mut config = AgentsConfig::default();
    config.codex.cli_path = PathBuf::from("/opt/bin/codex");
    config.claude.cli_path = PathBuf::from("/opt/bin/claude");
    config.codex.cloud.setup_secret_env = vec!["NPM_TOKEN".to_owned()];
    let values = configured_runtime_executables_from_agents_config(
        &config,
        source("codex-config"),
        source("claude-config"),
        IsolationTier::Host,
        &sandbox(SandboxMode::DangerFullAccess),
    );
    assert_eq!(values.len(), 3);
    assert_eq!(
        values[0].runtime_kind(),
        LocalExecutableRuntimeKind::CodexExec
    );
    assert_eq!(
        values[1].runtime_kind(),
        LocalExecutableRuntimeKind::CodexJsonrpc
    );
    assert_eq!(
        values[2].runtime_kind(),
        LocalExecutableRuntimeKind::ClaudeCode
    );
    assert_eq!(values[0].setup_secret_env(), [OsString::from("NPM_TOKEN")]);
    assert_eq!(values[2].executable(), Path::new("/opt/bin/claude"));
}
