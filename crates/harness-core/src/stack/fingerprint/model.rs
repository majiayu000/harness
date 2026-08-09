use super::schema::{
    canonicalize_typed_json, McpContractError, McpContractLimitKind, McpInputSchema,
    McpOutputSchema, McpToolAnnotations,
};
use super::{
    derive_mcp_tool_source, parse_mcp_tool_source, AgentStackFingerprintError,
    ConfiguredMcpServerBinding, RuntimeRoleSourceBinding,
    RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION,
};
use crate::stack::{AgentStackComponent, AgentStackSource, Sha256Digest};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

const MCP_IDENTITY_MAX_BYTES: usize = 1_024;
const MCP_DESCRIPTION_MAX_BYTES: usize = 65_536;

fn deserialize_digest<'de, D>(deserializer: D) -> Result<Sha256Digest, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Sha256Digest::parse(&value).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalExecutableRuntimeKind {
    CodexExec,
    CodexJsonrpc,
    ClaudeCode,
}

impl LocalExecutableRuntimeKind {
    pub const ALL: &'static [Self] = &[Self::CodexExec, Self::CodexJsonrpc, Self::ClaudeCode];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CodexExec => "codex_exec",
            Self::CodexJsonrpc => "codex_jsonrpc",
            Self::ClaudeCode => "claude_code",
        }
    }
    pub const fn version_args(self) -> &'static [&'static str] {
        let _ = self;
        &["--version"]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCommandForm {
    UnixBare,
    UnixAbsolute,
    UnixQualified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExecSequence {
    None,
    Single,
    EtxtbsyThenCheckpointAfter150Ms,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExecutionContext {
    LinuxFdCloexecExecveatEmptyPathFd10,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExecutableIdentity {
    file_size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
    #[serde(deserialize_with = "deserialize_digest")]
    executable_sha256: Sha256Digest,
    checkpoint_consistent_path: bool,
    exec_stop_consistent_handle: bool,
}

impl RuntimeExecutableIdentity {
    pub fn new(
        file_size_bytes: u64,
        unix_mode: Option<u32>,
        executable_sha256: Sha256Digest,
        checkpoint_consistent_path: bool,
        exec_stop_consistent_handle: bool,
    ) -> Self {
        Self {
            file_size_bytes,
            unix_mode,
            executable_sha256,
            checkpoint_consistent_path,
            exec_stop_consistent_handle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeVersionStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVersionFacts {
    normalized_version: String,
    #[serde(deserialize_with = "deserialize_digest")]
    stdout_sha256: Sha256Digest,
    #[serde(deserialize_with = "deserialize_digest")]
    stderr_sha256: Sha256Digest,
    selected_stream: RuntimeVersionStream,
}

impl RuntimeVersionFacts {
    pub fn new(
        normalized_version: String,
        stdout_sha256: Sha256Digest,
        stderr_sha256: Sha256Digest,
        selected_stream: RuntimeVersionStream,
    ) -> Result<Self, AgentStackFingerprintError> {
        if normalized_version.is_empty() || !normalized_version.is_ascii() {
            return Err(AgentStackFingerprintError::InvalidPayloadState);
        }
        Ok(Self {
            normalized_version,
            stdout_sha256,
            stderr_sha256,
            selected_stream,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RuntimeEnvironmentKey {
    #[serde(rename = "ANTHROPIC_API_KEY")]
    AnthropicApiKey,
    #[serde(rename = "CLAUDE_CONFIG_DIR")]
    ClaudeConfigDir,
    #[serde(rename = "OPENAI_API_KEY")]
    OpenaiApiKey,
    #[serde(rename = "PATH")]
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeEnvironmentValue {
    Unset,
    Redacted,
    SetDigest {
        #[serde(deserialize_with = "deserialize_digest")]
        value_sha256: Sha256Digest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEnvironmentFact {
    key: RuntimeEnvironmentKey,
    #[serde(flatten)]
    value: RuntimeEnvironmentValue,
}

impl RuntimeEnvironmentFact {
    pub fn new(key: RuntimeEnvironmentKey, value: RuntimeEnvironmentValue) -> Self {
        Self { key, value }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProbePhase {
    PathResolution,
    Identity,
    VersionProbe,
    LifecycleCleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProbeFailureKind {
    PathNotFound,
    PathUnusable,
    CandidateLimitExceeded,
    OpenFailed,
    MetadataUnavailable,
    NotRegularFile,
    NotExecutable,
    ExecutableTooLarge,
    ReadFailed,
    IdentityChanged,
    ProbeNotAuthorized,
    TargetAuthorizationUnavailable,
    InterpreterAuthorizationUnavailable,
    HandleExecutionUnavailable,
    SupervisionSetupFailed,
    SpawnFailed,
    TransitiveExecutionDenied,
    BareEaccesExhausted,
    Timeout,
    OutputLimitExceeded,
    OutputReadFailed,
    NonzeroExit,
    TerminatedBySignal,
    InvalidUtf8,
    EmptyOutput,
    UnparseableVersion,
    AmbiguousVersion,
    TerminationFailed,
    ReapFailed,
    OutputDrainFailed,
}

impl RuntimeProbeFailureKind {
    fn rank(self) -> u8 {
        self as u8
    }
    fn phase(self) -> RuntimeProbePhase {
        match self.rank() {
            0..=2 => RuntimeProbePhase::PathResolution,
            3..=9 => RuntimeProbePhase::Identity,
            10..=21 => RuntimeProbePhase::VersionProbe,
            _ => RuntimeProbePhase::LifecycleCleanup,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "detail", content = "value", rename_all = "snake_case")]
pub enum RuntimeProbeFailureDetail {
    ConfigurationSourceRepository,
    ResolvedTargetRepository,
    BoundaryUnprovable,
    LinkCountUnprovable,
    UnlinkedTarget,
    MultipleHardLinks,
    WorkingDirectoryEnter,
    TraceSetup,
    ProcessCreation,
    ImageExecution,
    ExecutableMapping,
    ExecutableImageMutation,
    KernelCodeLoading,
    ProcessSignalling,
    ExitCode(i32),
    OutputLimitBytes(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeProbeFailure {
    phase: RuntimeProbePhase,
    kind: RuntimeProbeFailureKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<RuntimeProbeFailureDetail>,
}

impl RuntimeProbeFailure {
    pub fn new(kind: RuntimeProbeFailureKind) -> Result<Self, AgentStackFingerprintError> {
        Self::build(kind, None)
    }
    pub fn with_detail(
        kind: RuntimeProbeFailureKind,
        detail: RuntimeProbeFailureDetail,
    ) -> Result<Self, AgentStackFingerprintError> {
        Self::build(kind, Some(detail))
    }
    fn build(
        kind: RuntimeProbeFailureKind,
        detail: Option<RuntimeProbeFailureDetail>,
    ) -> Result<Self, AgentStackFingerprintError> {
        let value = Self {
            phase: kind.phase(),
            kind,
            detail,
        };
        value
            .valid()
            .then_some(value)
            .ok_or(AgentStackFingerprintError::InvalidPayloadState)
    }
    fn valid(&self) -> bool {
        use RuntimeProbeFailureDetail as D;
        use RuntimeProbeFailureKind as K;
        self.phase == self.kind.phase()
            && match (&self.kind, &self.detail) {
                (
                    K::ProbeNotAuthorized,
                    Some(D::ConfigurationSourceRepository | D::ResolvedTargetRepository),
                ) => true,
                (
                    K::TargetAuthorizationUnavailable,
                    Some(
                        D::BoundaryUnprovable
                        | D::LinkCountUnprovable
                        | D::UnlinkedTarget
                        | D::MultipleHardLinks,
                    ),
                ) => true,
                (K::SupervisionSetupFailed, Some(D::WorkingDirectoryEnter | D::TraceSetup)) => true,
                (
                    K::TransitiveExecutionDenied,
                    Some(
                        D::ProcessCreation
                        | D::ImageExecution
                        | D::ExecutableMapping
                        | D::ExecutableImageMutation
                        | D::KernelCodeLoading
                        | D::ProcessSignalling,
                    ),
                ) => true,
                (K::NonzeroExit, Some(D::ExitCode(_))) => true,
                (K::OutputLimitExceeded, Some(D::OutputLimitBytes(_))) => true,
                (kind, None) => !matches!(
                    kind,
                    K::ProbeNotAuthorized
                        | K::TargetAuthorizationUnavailable
                        | K::SupervisionSetupFailed
                        | K::TransitiveExecutionDenied
                        | K::NonzeroExit
                        | K::OutputLimitExceeded
                ),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeResolutionAttemptOutcome {
    Absent,
    NotRegular,
    NotExecutable,
    InspectionFailed,
    InspectionTarget,
    AuthorizationUnavailable,
    InterpreterAuthorizationUnavailable,
    HandleExecutionUnavailable,
    SupervisionSetupFailed,
    RetryNotAuthorized,
    RetryAuthorizationUnavailable,
    ExecVerificationFailed,
    ExecEacces,
    ExecFailed,
    ExecStarted,
}

impl RuntimeResolutionAttemptOutcome {
    fn terminal(self) -> bool {
        !matches!(
            self,
            Self::Absent | Self::NotRegular | Self::NotExecutable | Self::ExecEacces
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeResolutionAttempt {
    #[serde(deserialize_with = "deserialize_digest")]
    candidate_digest: Sha256Digest,
    outcome: RuntimeResolutionAttemptOutcome,
    exec_sequence: RuntimeExecSequence,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_context: Option<RuntimeExecutionContext>,
}

impl RuntimeResolutionAttempt {
    pub fn new(
        candidate_digest: Sha256Digest,
        outcome: RuntimeResolutionAttemptOutcome,
        exec_sequence: RuntimeExecSequence,
        exec_context: Option<RuntimeExecutionContext>,
    ) -> Result<Self, AgentStackFingerprintError> {
        if (exec_sequence == RuntimeExecSequence::None) != exec_context.is_none() {
            return Err(AgentStackFingerprintError::InvalidPayloadState);
        }
        Ok(Self {
            candidate_digest,
            outcome,
            exec_sequence,
            exec_context,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeExecutableFingerprintPayload {
    schema_version: &'static str,
    runtime_kind: LocalExecutableRuntimeKind,
    execution_isolation: &'static str,
    sandbox_policy: &'static str,
    command_form: RuntimeCommandForm,
    configured_command_digest: Sha256Digest,
    working_directory_digest: Sha256Digest,
    working_directory_identity_digest: Sha256Digest,
    resolution_attempts: Vec<RuntimeResolutionAttempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executable: Option<RuntimeExecutableIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<RuntimeVersionFacts>,
    environment: Vec<RuntimeEnvironmentFact>,
    failures: Vec<RuntimeProbeFailure>,
    #[serde(skip)]
    role_binding: RuntimeRoleSourceBinding,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimePayloadWire {
    schema_version: String,
    runtime_kind: LocalExecutableRuntimeKind,
    execution_isolation: String,
    sandbox_policy: String,
    command_form: RuntimeCommandForm,
    #[serde(deserialize_with = "deserialize_digest")]
    configured_command_digest: Sha256Digest,
    #[serde(deserialize_with = "deserialize_digest")]
    working_directory_digest: Sha256Digest,
    #[serde(deserialize_with = "deserialize_digest")]
    working_directory_identity_digest: Sha256Digest,
    resolution_attempts: Vec<RuntimeResolutionAttempt>,
    executable: Option<RuntimeExecutableIdentity>,
    version: Option<RuntimeVersionFacts>,
    environment: Vec<RuntimeEnvironmentFact>,
    failures: Vec<RuntimeProbeFailure>,
}

impl RuntimeExecutableFingerprintPayload {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role_binding: RuntimeRoleSourceBinding,
        command_form: RuntimeCommandForm,
        configured_command_digest: Sha256Digest,
        working_directory_digest: Sha256Digest,
        working_directory_identity_digest: Sha256Digest,
        resolution_attempts: Vec<RuntimeResolutionAttempt>,
        executable: Option<RuntimeExecutableIdentity>,
        version: Option<RuntimeVersionFacts>,
        environment: Vec<RuntimeEnvironmentFact>,
        failures: Vec<RuntimeProbeFailure>,
    ) -> Result<Self, AgentStackFingerprintError> {
        let value = Self {
            schema_version: RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION,
            runtime_kind: role_binding.runtime_kind(),
            execution_isolation: "host",
            sandbox_policy: "danger_full_access_unrestricted",
            command_form,
            configured_command_digest,
            working_directory_digest,
            working_directory_identity_digest,
            resolution_attempts,
            executable,
            version,
            environment,
            failures,
            role_binding,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn from_wire(
        wire: RuntimePayloadWire,
        component: &AgentStackComponent,
    ) -> Result<Self, AgentStackFingerprintError> {
        if wire.schema_version != RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION
            || wire.execution_isolation != "host"
            || wire.sandbox_policy != "danger_full_access_unrestricted"
        {
            return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
        }
        Self::new(
            RuntimeRoleSourceBinding::parse(
                component.source(),
                wire.runtime_kind,
                component.integrity().cloned(),
            )?,
            wire.command_form,
            wire.configured_command_digest,
            wire.working_directory_digest,
            wire.working_directory_identity_digest,
            wire.resolution_attempts,
            wire.executable,
            wire.version,
            wire.environment,
            wire.failures,
        )
    }

    pub(crate) fn validate(&self) -> Result<(), AgentStackFingerprintError> {
        let attempts_valid = self.resolution_attempts.len() <= 64
            && self
                .resolution_attempts
                .iter()
                .enumerate()
                .all(|(index, attempt)| {
                    !attempt.outcome.terminal() || index + 1 == self.resolution_attempts.len()
                });
        let failures_valid = self.failures.iter().all(RuntimeProbeFailure::valid)
            && self.failures.windows(2).all(|pair| {
                (pair[0].phase, pair[0].kind.rank()) < (pair[1].phase, pair[1].kind.rank())
            });
        let version_has_stable_identity = self.version.is_none()
            || self.executable.as_ref().is_some_and(|identity| {
                identity.checkpoint_consistent_path && identity.exec_stop_consistent_handle
            });
        if self.runtime_kind != self.role_binding.runtime_kind()
            || !attempts_valid
            || !failures_valid
            || !valid_environment(self.runtime_kind, &self.environment)
            || !version_has_stable_identity
        {
            return Err(AgentStackFingerprintError::InvalidPayloadState);
        }
        Ok(())
    }
    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, AgentStackFingerprintError> {
        canonicalize_typed_json(self).map_err(Into::into)
    }
    pub fn role_binding(&self) -> &RuntimeRoleSourceBinding {
        &self.role_binding
    }
    pub const fn runtime_kind(&self) -> LocalExecutableRuntimeKind {
        self.runtime_kind
    }
}

fn valid_environment(kind: LocalExecutableRuntimeKind, facts: &[RuntimeEnvironmentFact]) -> bool {
    facts.windows(2).all(|pair| pair[0].key < pair[1].key)
        && facts.iter().all(|fact| {
            let key_allowed = match kind {
                LocalExecutableRuntimeKind::CodexExec
                | LocalExecutableRuntimeKind::CodexJsonrpc => matches!(
                    fact.key,
                    RuntimeEnvironmentKey::OpenaiApiKey | RuntimeEnvironmentKey::Path
                ),
                LocalExecutableRuntimeKind::ClaudeCode => matches!(
                    fact.key,
                    RuntimeEnvironmentKey::AnthropicApiKey
                        | RuntimeEnvironmentKey::ClaudeConfigDir
                        | RuntimeEnvironmentKey::Path
                ),
            };
            let state_allowed = match fact.key {
                RuntimeEnvironmentKey::AnthropicApiKey | RuntimeEnvironmentKey::OpenaiApiKey => {
                    matches!(
                        fact.value,
                        RuntimeEnvironmentValue::Unset | RuntimeEnvironmentValue::Redacted
                    )
                }
                _ => !matches!(fact.value, RuntimeEnvironmentValue::Redacted),
            };
            key_allowed && state_allowed
        })
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolFingerprintPayload {
    server_binding: ConfiguredMcpServerBinding,
    tool_source: AgentStackSource,
    tool_name: String,
    description: Option<String>,
    annotations: Option<McpToolAnnotations>,
    input_schema: McpInputSchema,
    output_schema: Option<McpOutputSchema>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpPayloadWire<'a> {
    schema_version: String,
    server_component_id: String,
    tool_name: String,
    description: Option<String>,
    #[serde(default, borrow)]
    annotations: Option<&'a RawValue>,
    #[serde(rename = "inputSchema", borrow)]
    input_schema: &'a RawValue,
    #[serde(default, rename = "outputSchema", borrow)]
    output_schema: Option<&'a RawValue>,
}

impl McpToolFingerprintPayload {
    pub fn new(
        server_binding: ConfiguredMcpServerBinding,
        tool_name: &str,
        description: Option<&str>,
        annotations: Option<McpToolAnnotations>,
        input_schema: McpInputSchema,
        output_schema: Option<McpOutputSchema>,
    ) -> Result<Self, AgentStackFingerprintError> {
        validate_identity(tool_name, McpContractLimitKind::ToolNameBytes)?;
        if description.is_some_and(|value| value.len() > MCP_DESCRIPTION_MAX_BYTES) {
            return Err(
                McpContractError::LimitExceeded(McpContractLimitKind::DescriptionBytes).into(),
            );
        }
        let tool_source =
            derive_mcp_tool_source(server_binding.server_source(), tool_name.as_bytes())?;
        Ok(Self {
            server_binding,
            tool_source,
            tool_name: tool_name.to_owned(),
            description: description.map(str::to_owned),
            annotations,
            input_schema,
            output_schema,
        })
    }

    pub(crate) fn from_wire(
        wire: McpPayloadWire<'_>,
        component: &AgentStackComponent,
    ) -> Result<Self, AgentStackFingerprintError> {
        if wire.schema_version != super::MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION {
            return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
        }
        let (base, stable_key, tool_name) = parse_mcp_tool_source(component.source())?;
        if tool_name != wire.tool_name {
            return Err(AgentStackFingerprintError::InvalidComponentBinding);
        }
        let binding = ConfiguredMcpServerBinding::new(base, &stable_key)?;
        if binding.server_component_id().as_str() != wire.server_component_id {
            return Err(AgentStackFingerprintError::InvalidComponentBinding);
        }
        Self::new(
            binding,
            &wire.tool_name,
            wire.description.as_deref(),
            wire.annotations
                .map(|value| McpToolAnnotations::from_json_str(value.get()))
                .transpose()?,
            McpInputSchema::from_json_str(wire.input_schema.get())?,
            wire.output_schema
                .map(|value| McpOutputSchema::from_json_str(value.get()))
                .transpose()?,
        )
    }

    pub(crate) fn validate(&self) -> Result<(), AgentStackFingerprintError> {
        if derive_mcp_tool_source(
            self.server_binding.server_source(),
            self.tool_name.as_bytes(),
        )? == self.tool_source
        {
            Ok(())
        } else {
            Err(AgentStackFingerprintError::InvalidComponentBinding)
        }
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, AgentStackFingerprintError> {
        let mut fields = vec![
            ("inputSchema", self.input_schema.canonical_bytes()),
            (
                "schema_version",
                json_string(super::MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION)?,
            ),
            (
                "server_component_id",
                json_string(self.server_binding.server_component_id().as_str())?,
            ),
            ("tool_name", json_string(&self.tool_name)?),
        ];
        if let Some(value) = &self.description {
            fields.push(("description", json_string(value)?));
        }
        if let Some(value) = &self.annotations {
            fields.push(("annotations", value.canonical_bytes()));
        }
        if let Some(value) = &self.output_schema {
            fields.push(("outputSchema", value.canonical_bytes()));
        }
        fields.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
        let mut output = vec![b'{'];
        for (index, (key, value)) in fields.into_iter().enumerate() {
            if index != 0 {
                output.push(b',');
            }
            output.extend_from_slice(&json_string(key)?);
            output.push(b':');
            output.extend_from_slice(&value);
        }
        output.push(b'}');
        Ok(output)
    }
    pub fn server_binding(&self) -> &ConfiguredMcpServerBinding {
        &self.server_binding
    }
    pub(crate) fn tool_source(&self) -> &AgentStackSource {
        &self.tool_source
    }
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

impl Serialize for McpToolFingerprintPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            schema_version: &'static str,
            server_component_id: &'a str,
            tool_name: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            annotations: Option<&'a McpToolAnnotations>,
            #[serde(rename = "inputSchema")]
            input_schema: &'a McpInputSchema,
            #[serde(skip_serializing_if = "Option::is_none", rename = "outputSchema")]
            output_schema: Option<&'a McpOutputSchema>,
        }
        Wire {
            schema_version: super::MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION,
            server_component_id: self.server_binding.server_component_id().as_str(),
            tool_name: &self.tool_name,
            description: self.description.as_deref(),
            annotations: self.annotations.as_ref(),
            input_schema: &self.input_schema,
            output_schema: self.output_schema.as_ref(),
        }
        .serialize(serializer)
    }
}

fn json_string(value: &str) -> Result<Vec<u8>, AgentStackFingerprintError> {
    serde_json::to_vec(value).map_err(AgentStackFingerprintError::Json)
}

fn validate_identity(
    value: &str,
    kind: McpContractLimitKind,
) -> Result<(), AgentStackFingerprintError> {
    if value.len() > MCP_IDENTITY_MAX_BYTES {
        return Err(McpContractError::LimitExceeded(kind).into());
    }
    if value.is_empty()
        || value
            .bytes()
            .all(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b' '))
    {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    Ok(())
}
