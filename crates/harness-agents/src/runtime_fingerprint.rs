//! Fail-closed production boundary for local runtime fingerprints.

#[cfg(test)]
mod tests;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod authorization;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod candidate;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod capability;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod checkpoint;
mod command;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod completion;
mod environment;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod exec_stop;
mod executable;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod launch;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod owner;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod probe;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod registry;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod resolution;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod supervision;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod syscall_guard;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod target;
#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod test_fixtures;
mod windows_candidate;
mod windows_resolution;

use harness_core::config::agents::{AgentsConfig, SandboxMode};
use harness_core::config::isolation::IsolationTier;
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, ConfiguredRuntimeSource, LocalExecutableRuntimeKind,
};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use harness_core::stack::fingerprint::{
    RuntimeCommandForm, RuntimeEnvironmentFact, RuntimeExecutableFingerprintPayload,
    RuntimeExecutableIdentity, RuntimeProbeFailure, RuntimeResolutionAttempt,
    RuntimeRoleSourceBinding, RuntimeVersionFacts,
};
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use harness_core::stack::Sha256Digest;
use harness_sandbox::{NetworkPolicy, SandboxSpec};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

pub use environment::{
    classify_completed_runtime_output, windows_working_directory_digest,
    RuntimeOutputClassification, RuntimeTermination,
};
pub use executable::{
    classify_static_linux_elf, runtime_working_directory_identity_digest, LinuxElfArchitecture,
    LinuxStaticElfClassification,
};
pub use harness_core::stack::fingerprint::{
    RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES, RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES,
};
pub use windows_candidate::WindowsResolvedCandidate;
pub use windows_resolution::{
    resolve_windows_command, WindowsResolution, WindowsResolutionContextEvidence,
    WindowsResolutionInput,
};

pub const RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS: usize = 65_536;
pub const RUNTIME_FINGERPRINT_MAX_OBSERVATION_ENV_ENTRIES: usize = 1_024;
pub const RUNTIME_FINGERPRINT_MAX_ENVIRONMENT_KEY_UNITS: usize = 1_024;
pub const RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAMES: usize = 1_024;
pub const RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAME_UNITS: usize = 1_024;
pub const RUNTIME_FINGERPRINT_MAX_RESOLUTION_CANDIDATES: usize = 64;
pub const RUNTIME_FINGERPRINT_OWNER_CAPACITY: usize = 8;
pub const RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS: usize = 2;
pub const RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS: usize = 28;
pub const RUNTIME_FINGERPRINT_POST_READY_CHILD_REFERENCES: usize = 12;
pub const RUNTIME_FINGERPRINT_TARGET_EXEC_FD: i32 = 10;
pub const RUNTIME_FINGERPRINT_PROBE_DEADLINE: Duration = Duration::from_secs(5);
pub const RUNTIME_FINGERPRINT_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
pub const RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE: Duration = Duration::from_secs(1);
pub const RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE: Duration = Duration::from_secs(1);
pub const RUNTIME_FINGERPRINT_ETXTBSY_RETRY_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRepositoryBoundarySet {
    roots: Vec<PathBuf>,
}

impl ValidatedRepositoryBoundarySet {
    pub fn from_existing_roots(
        declared_repository_root: impl AsRef<Path>,
        linked_worktree_roots: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> Result<Self, RuntimeFingerprintProduceError> {
        let declared = canonical_repository_directory(declared_repository_root.as_ref())?;
        let mut roots = vec![declared];
        for root in linked_worktree_roots {
            let canonical = canonical_repository_directory(root.as_ref())?;
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        roots.sort();
        Ok(Self { roots })
    }

    pub fn contains(&self, target: &Path) -> bool {
        self.roots.iter().any(|root| target.starts_with(root))
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

fn canonical_repository_directory(root: &Path) -> Result<PathBuf, RuntimeFingerprintProduceError> {
    let canonical = std::fs::canonicalize(root)
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    if !std::fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    Ok(canonical)
}

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
pub enum RuntimeLaunchInputLimitKind {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeOwnedChildRole {
    Observation(RuntimeObservationStage),
    InitialTarget,
    RetryTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeObservationStage {
    CapabilityCheck,
    WorkingDirectory,
    Candidate,
    TargetAuthorization,
    SourceHash,
    PreSpawnCheckpoint,
    ExecStopCheckpoint,
    PostReapCheckpoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeChildRegistrationStage {
    GateCreate,
    Fork,
    SignalIsolation,
    DescriptorIsolation,
    PidfdOpen,
    RegistryCommit,
    GateRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeChildCleanupOperation {
    GateClose,
    Termination,
    Reap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeObservationCleanupOperation {
    Termination,
    Reap,
    ProtocolClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeObservationProtocolReason {
    TruncatedFrame,
    OversizedFrame,
    SurplusFields,
    DescriptorCountMismatch,
    HelperExited,
}

#[derive(Debug, Error)]
pub enum RuntimeFingerprintProduceError {
    #[error(transparent)]
    Stack(#[from] harness_core::stack::fingerprint::AgentStackFingerprintError),
    #[error("runtime isolation {0:?} cannot produce a host fingerprint")]
    UnsupportedIsolation(IsolationTier),
    #[error("the effective sandbox does not match unrestricted host execution")]
    SandboxParityUnavailable,
    #[error("runtime fingerprint containment is unavailable: {0:?}")]
    ContainmentUnavailable(ContainmentUnavailableReason),
    #[error("runtime fingerprint launch input exceeds the {0:?} limit")]
    LaunchInputLimitExceeded(RuntimeLaunchInputLimitKind),
    #[error("runtime fingerprint output limit must be in 1..=65536")]
    InvalidOutputLimit,
    #[error("runtime fingerprint environment contains an invalid key")]
    InvalidEnvironmentKey,
    #[error("runtime fingerprint environment contains a canonical key collision")]
    EnvironmentKeyCollision,
    #[error("runtime fingerprint launch context is invalid")]
    InvalidLaunchContext,
    #[error("runtime fingerprint working directory is unavailable")]
    WorkingDirectoryUnavailable,
    #[error("runtime fingerprint owner resource capacity is exhausted")]
    OwnerResourceCapacityExceeded,
    #[error("runtime child registration failed for {role:?} at {stage:?}")]
    ChildRegistrationUnavailable {
        role: RuntimeOwnedChildRole,
        stage: RuntimeChildRegistrationStage,
    },
    #[error("runtime child registration cleanup is incomplete for {role:?}: {operation:?}")]
    ChildRegistrationCleanupIncomplete {
        role: RuntimeOwnedChildRole,
        operation: RuntimeChildCleanupOperation,
    },
    #[error("runtime observation deadline exceeded at {stage:?}")]
    ObservationDeadlineExceeded { stage: RuntimeObservationStage },
    #[error("runtime observation cleanup is incomplete at {stage:?}: {operation:?}")]
    ObservationCleanupIncomplete {
        stage: RuntimeObservationStage,
        operation: RuntimeObservationCleanupOperation,
    },
    #[error("runtime observation protocol is invalid at {stage:?}: {reason:?}")]
    ObservationProtocolInvalid {
        stage: RuntimeObservationStage,
        reason: RuntimeObservationProtocolReason,
    },
    #[error("runtime execution verification is unavailable")]
    ExecutionVerificationUnavailable,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) struct RuntimeEnvelopeEvidence {
    pub(super) command_form: RuntimeCommandForm,
    pub(super) configured_command_digest: Sha256Digest,
    pub(super) working_directory_digest: Sha256Digest,
    pub(super) working_directory_identity_digest: Sha256Digest,
    pub(super) resolution_attempts: Vec<RuntimeResolutionAttempt>,
    pub(super) executable: Option<RuntimeExecutableIdentity>,
    pub(super) version: Option<RuntimeVersionFacts>,
    pub(super) environment: Vec<RuntimeEnvironmentFact>,
    pub(super) failures: Vec<RuntimeProbeFailure>,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn finish_runtime_envelope(
    configured: &ConfiguredRuntimeExecutable,
    evidence: RuntimeEnvelopeEvidence,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    let role_binding = RuntimeRoleSourceBinding::derive(
        configured.configured_source(),
        configured.runtime_kind(),
    )?;
    let payload = RuntimeExecutableFingerprintPayload::new(
        role_binding,
        evidence.command_form,
        evidence.configured_command_digest,
        evidence.working_directory_digest,
        evidence.working_directory_identity_digest,
        evidence.resolution_attempts,
        evidence.executable,
        evidence.version,
        evidence.environment,
        evidence.failures,
    )?;
    Ok(AgentStackFingerprintEnvelope::agent_runtime(payload)?)
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

    fn validate_execution_boundary(&self) -> Result<(), RuntimeFingerprintProduceError> {
        if self.isolation != IsolationTier::Host {
            return Err(RuntimeFingerprintProduceError::UnsupportedIsolation(
                self.isolation,
            ));
        }
        if self.sandbox.mode != SandboxMode::DangerFullAccess
            || self.sandbox.allowed_write_paths.is_some()
            || self.sandbox.network_policy != NetworkPolicy::InheritSandboxMode
        {
            return Err(RuntimeFingerprintProduceError::SandboxParityUnavailable);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeFingerprintOptions {
    working_dir: PathBuf,
    environment: Vec<(OsString, OsString)>,
    repository_boundaries: Option<ValidatedRepositoryBoundarySet>,
    max_output_bytes: usize,
}

impl RuntimeFingerprintOptions {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            environment: Vec::new(),
            repository_boundaries: None,
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

    pub fn with_repository_boundaries(
        mut self,
        repository_boundaries: ValidatedRepositoryBoundarySet,
    ) -> Self {
        self.repository_boundaries = Some(repository_boundaries);
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

    pub fn repository_boundaries(&self) -> Option<&ValidatedRepositoryBoundarySet> {
        self.repository_boundaries.as_ref()
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

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub async fn fingerprint_configured_runtime_executable(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    executable.validate_execution_boundary()?;
    validate_output_limit(options.max_output_bytes)?;
    validate_launch_value_limit(
        executable.executable.as_os_str(),
        RuntimeLaunchInputLimitKind::ConfiguredCommand,
    )?;
    validate_launch_value_limit(
        options.working_dir.as_os_str(),
        RuntimeLaunchInputLimitKind::WorkingDirectory,
    )?;
    let selected_environment = environment::validate_and_select(
        executable.runtime_kind,
        &options.environment,
        &executable.setup_secret_env,
    )?;
    let prepared_command = command::prepare_command(
        executable.executable.as_os_str(),
        &options.working_dir,
        selected_environment.child_path.as_deref(),
    )?;
    if !prepared_command.validate_shape() {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    owner::run(executable, options, selected_environment, prepared_command).await
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn validate_output_limit(max_output_bytes: usize) -> Result<(), RuntimeFingerprintProduceError> {
    if (1..=RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES).contains(&max_output_bytes) {
        Ok(())
    } else {
        Err(RuntimeFingerprintProduceError::InvalidOutputLimit)
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub async fn fingerprint_configured_runtime_executable(
    executable: &ConfiguredRuntimeExecutable,
    _options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    executable.validate_execution_boundary()?;
    Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
        ContainmentUnavailableReason::UnsupportedPlatform,
    ))
}

#[cfg(all(
    unix,
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
const _: fn(
    &std::ffi::OsStr,
    &Path,
    Option<&std::ffi::OsStr>,
) -> Result<command::PreparedCommand, RuntimeFingerprintProduceError> = command::prepare_command;
#[cfg(all(
    unix,
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
const _: fn(&command::PreparedCommand) -> bool = command::PreparedCommand::validate_shape;
#[cfg(all(
    unix,
    not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
const _: fn(&std::ffi::OsStr, usize) -> usize = executable::native_os_units_len;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn validate_launch_value_limit(
    value: &std::ffi::OsStr,
    kind: RuntimeLaunchInputLimitKind,
) -> Result<(), RuntimeFingerprintProduceError> {
    if executable::native_os_units_len(value, RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS)
        > RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS
    {
        Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            kind,
        ))
    } else {
        Ok(())
    }
}
