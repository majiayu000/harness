//! Fail-closed production boundary for local runtime fingerprints.

#[cfg(test)]
mod tests;

use harness_core::config::agents::{AgentsConfig, SandboxMode};
use harness_core::config::isolation::IsolationTier;
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, ConfiguredRuntimeSource, LocalExecutableRuntimeKind,
};
use harness_sandbox::SandboxSpec;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES: u64 = 67_108_864;
pub const RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES: usize = 65_536;
pub const RUNTIME_FINGERPRINT_ACTIVE_DEADLINE_SECS: u64 = 5;
pub const RUNTIME_FINGERPRINT_CLEANUP_DEADLINE_SECS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentUnavailableReason {
    UnsupportedPlatform,
    OwnerCapacityExhausted,
    OwnerStartFailed,
    OwnerReadyTimeout,
    OwnerStopJoinTimeout,
    SignalIsolationUnavailable,
    DescriptorIsolationUnavailable,
    PidfdUnavailable,
    PostExecGuardUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchInputLimitKind {
    ConfiguredCommand,
    WorkingDirectory,
    WindowsCurrentExecutableDirectory,
    WindowsSystemDirectory,
    WindowsDirectory,
    WindowsParentPath,
    ObservationEnvironmentEntries,
    EnvironmentKey,
    SetupSecretNames,
    SetupSecretName,
    ChildPath,
    ClaudeConfigDirectory,
}

#[derive(Debug, Error)]
pub enum RuntimeFingerprintError {
    #[error(transparent)]
    Stack(#[from] harness_core::stack::fingerprint::AgentStackFingerprintError),
    #[error("runtime isolation {0:?} cannot produce a host fingerprint")]
    UnsupportedIsolation(IsolationTier),
    #[error("the effective sandbox does not match unrestricted host execution")]
    SandboxParityUnavailable,
    #[error("runtime fingerprint containment is unavailable: {0:?}")]
    ContainmentUnavailable(ContainmentUnavailableReason),
    #[error("runtime fingerprint launch input exceeds the {0:?} limit")]
    LaunchInputLimitExceeded(LaunchInputLimitKind),
    #[error("runtime fingerprint output limit must be in 1..=65536")]
    InvalidOutputLimit,
}

#[derive(Debug, Clone)]
pub struct ConfiguredRuntimeExecutable {
    runtime_kind: LocalExecutableRuntimeKind,
    configured_source: ConfiguredRuntimeSource,
    isolation: IsolationTier,
    sandbox: SandboxSpec,
    executable: PathBuf,
    setup_secret_env: Vec<OsString>,
}

impl ConfiguredRuntimeExecutable {
    pub fn new(
        runtime_kind: LocalExecutableRuntimeKind,
        configured_source: ConfiguredRuntimeSource,
        isolation: IsolationTier,
        sandbox: SandboxSpec,
        executable: impl Into<PathBuf>,
        setup_secret_env: Vec<OsString>,
    ) -> Self {
        Self {
            runtime_kind,
            configured_source,
            isolation,
            sandbox,
            executable: executable.into(),
            setup_secret_env,
        }
    }

    pub const fn runtime_kind(&self) -> LocalExecutableRuntimeKind {
        self.runtime_kind
    }

    pub fn configured_source(&self) -> &ConfiguredRuntimeSource {
        &self.configured_source
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn setup_secret_env(&self) -> &[OsString] {
        &self.setup_secret_env
    }

    fn validate_execution_boundary(&self) -> Result<(), RuntimeFingerprintError> {
        if self.isolation != IsolationTier::Host {
            return Err(RuntimeFingerprintError::UnsupportedIsolation(
                self.isolation,
            ));
        }
        if self.sandbox.mode != SandboxMode::DangerFullAccess
            || self.sandbox.allowed_write_paths.is_some()
        {
            return Err(RuntimeFingerprintError::SandboxParityUnavailable);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeFingerprintOptions {
    working_dir: PathBuf,
    environment: Vec<(OsString, OsString)>,
    max_output_bytes: usize,
}

impl RuntimeFingerprintOptions {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            environment: Vec::new(),
            max_output_bytes: RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES,
        }
    }

    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        self.environment = environment.into_iter().collect();
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    pub fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }

    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

pub fn configured_runtime_executables_from_agents_config(
    config: &AgentsConfig,
    codex_source: ConfiguredRuntimeSource,
    claude_source: ConfiguredRuntimeSource,
    isolation: IsolationTier,
    sandbox: &SandboxSpec,
) -> Vec<ConfiguredRuntimeExecutable> {
    let setup_secret_env = config
        .codex
        .cloud
        .setup_secret_env
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    vec![
        ConfiguredRuntimeExecutable::new(
            LocalExecutableRuntimeKind::CodexExec,
            codex_source.clone(),
            isolation,
            sandbox.clone(),
            config.codex.cli_path.clone(),
            setup_secret_env.clone(),
        ),
        ConfiguredRuntimeExecutable::new(
            LocalExecutableRuntimeKind::CodexJsonrpc,
            codex_source,
            isolation,
            sandbox.clone(),
            config.codex.cli_path.clone(),
            setup_secret_env,
        ),
        ConfiguredRuntimeExecutable::new(
            LocalExecutableRuntimeKind::ClaudeCode,
            claude_source,
            isolation,
            sandbox.clone(),
            config.claude.cli_path.clone(),
            Vec::new(),
        ),
    ]
}

pub async fn fingerprint_configured_runtime_executable(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintError> {
    executable.validate_execution_boundary()?;
    produce_on_supported_platform(executable, options).await
}

#[cfg(not(target_os = "linux"))]
async fn produce_on_supported_platform(
    _executable: &ConfiguredRuntimeExecutable,
    _options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintError> {
    Err(RuntimeFingerprintError::ContainmentUnavailable(
        ContainmentUnavailableReason::UnsupportedPlatform,
    ))
}

#[cfg(target_os = "linux")]
async fn produce_on_supported_platform(
    _executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintError> {
    if !(1..=RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES).contains(&options.max_output_bytes) {
        return Err(RuntimeFingerprintError::InvalidOutputLimit);
    }
    Err(RuntimeFingerprintError::ContainmentUnavailable(
        ContainmentUnavailableReason::PostExecGuardUnavailable,
    ))
}
