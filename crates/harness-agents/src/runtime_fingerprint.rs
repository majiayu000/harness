//! Fail-closed production boundary for local runtime fingerprints.

#[cfg(test)]
mod tests;

#[cfg(target_os = "linux")]
mod authorization;
#[cfg(target_os = "linux")]
mod candidate;
#[cfg(target_os = "linux")]
mod checkpoint;
mod environment;
mod executable;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod resolution;

use harness_core::config::agents::{AgentsConfig, SandboxMode};
use harness_core::config::isolation::IsolationTier;
use harness_core::stack::fingerprint::{
    AgentStackFingerprintEnvelope, ConfiguredRuntimeSource, LocalExecutableRuntimeKind,
};
#[cfg(target_os = "linux")]
use harness_core::stack::fingerprint::{
    RuntimeCommandForm, RuntimeEnvironmentFact, RuntimeExecutableFingerprintPayload,
    RuntimeExecutableIdentity, RuntimeProbeFailure, RuntimeResolutionAttempt,
    RuntimeRoleSourceBinding, RuntimeVersionFacts,
};
#[cfg(target_os = "linux")]
use harness_core::stack::Sha256Digest;
use harness_sandbox::SandboxSpec;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;

pub use environment::{
    classify_completed_runtime_output, windows_working_directory_digest,
    RuntimeOutputClassification, RuntimeTermination,
};
pub use executable::{
    classify_static_linux_elf, runtime_working_directory_identity_digest, LinuxElfArchitecture,
    LinuxStaticElfClassification,
};

pub const RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES: u64 = 67_108_864;
pub const RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES: usize = 65_536;
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
        let declared = std::fs::canonicalize(declared_repository_root)
            .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
        let mut roots = vec![declared];
        for root in linked_worktree_roots {
            let canonical = std::fs::canonicalize(root)
                .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
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

#[cfg(target_os = "linux")]
static ACTIVE_RUNTIME_FINGERPRINT_OWNERS: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "linux")]
struct RuntimeFingerprintOwnerPermit;

#[cfg(target_os = "linux")]
impl RuntimeFingerprintOwnerPermit {
    fn try_acquire() -> Result<Self, RuntimeFingerprintProduceError> {
        ACTIVE_RUNTIME_FINGERPRINT_OWNERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < RUNTIME_FINGERPRINT_OWNER_CAPACITY).then_some(active + 1)
            })
            .map_err(|_| RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)?;
        Ok(Self)
    }
}

#[cfg(target_os = "linux")]
impl Drop for RuntimeFingerprintOwnerPermit {
    fn drop(&mut self) {
        ACTIVE_RUNTIME_FINGERPRINT_OWNERS.fetch_sub(1, Ordering::AcqRel);
    }
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

pub async fn fingerprint_configured_runtime_executable(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    executable.validate_execution_boundary()?;
    ensure_supported_platform()?;
    if !(1..=RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES).contains(&options.max_output_bytes) {
        return Err(RuntimeFingerprintProduceError::InvalidOutputLimit);
    }
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
    let prepared_command = executable::prepare_command(
        executable.executable.as_os_str(),
        &options.working_dir,
        selected_environment.child_path.as_deref(),
    )?;
    if !prepared_command.validate_shape() {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    produce_on_supported_platform(executable, options, selected_environment, prepared_command).await
}

#[cfg(target_os = "linux")]
async fn produce_on_supported_platform(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    selected_environment: environment::SelectedEnvironment,
    prepared_command: executable::PreparedCommand,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    let permit = RuntimeFingerprintOwnerPermit::try_acquire()?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let owner_stop = Arc::clone(&stop_requested);
    let executable = executable.clone();
    let options = options.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("runtime-fingerprint-owner".to_owned())
        .spawn(move || {
            let _permit = permit;
            if ready_tx.send(()).is_err() || owner_stop.load(Ordering::Acquire) {
                return;
            }
            let result = probe::owner_run(
                &executable,
                &options,
                selected_environment,
                prepared_command,
                &owner_stop,
            );
            if result_tx.send(result).is_err() {
                tracing::error!("runtime fingerprint caller dropped before owner completion");
            }
        })
        .map_err(|_| {
            RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerStartFailed,
            )
        })?;
    match tokio::time::timeout(RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE, ready_rx).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerStartFailed,
            ));
        }
        Err(_) => {
            stop_requested.store(true, Ordering::Release);
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::OwnerReadyTimeout,
            ));
        }
    }
    result_rx.await.map_err(|_| {
        RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::OwnerStopJoinTimeout,
        )
    })?
}

#[cfg(not(target_os = "linux"))]
async fn produce_on_supported_platform(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    selected_environment: environment::SelectedEnvironment,
    prepared_command: executable::PreparedCommand,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    let environment::SelectedEnvironment { facts, child_path } = selected_environment;
    let executable::PreparedCommand {
        command_form,
        configured_command_digest,
        working_directory_digest,
        candidates,
        candidate_limit_exceeded,
        path_unusable,
    } = prepared_command;
    for candidate in candidates {
        match candidate.reference {
            executable::CandidateReference::Absolute(path)
            | executable::CandidateReference::WorkingDirectoryRelative(path) => drop(path),
        }
        drop(candidate.candidate_digest);
    }
    drop((
        executable,
        options,
        facts,
        child_path,
        command_form,
        configured_command_digest,
        working_directory_digest,
        candidate_limit_exceeded,
        path_unusable,
    ));
    unreachable!("the platform gate returns before producer dispatch")
}

#[cfg(target_os = "linux")]
fn ensure_supported_platform() -> Result<(), RuntimeFingerprintProduceError> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_supported_platform() -> Result<(), RuntimeFingerprintProduceError> {
    Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
        ContainmentUnavailableReason::UnsupportedPlatform,
    ))
}

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
