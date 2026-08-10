//! Bounded, source-preserving Agent Stack fingerprints.

mod model;
mod schema;
#[cfg(test)]
mod tests;

pub use model::{
    LocalExecutableRuntimeKind, McpToolFingerprintPayload, RuntimeCommandForm,
    RuntimeEnvironmentFact, RuntimeEnvironmentKey, RuntimeEnvironmentValue, RuntimeExecSequence,
    RuntimeExecutableFingerprintPayload, RuntimeExecutableIdentity, RuntimeExecutionContext,
    RuntimeProbeFailure, RuntimeProbeFailureDetail, RuntimeProbeFailureKind, RuntimeProbePhase,
    RuntimeResolutionAttempt, RuntimeResolutionAttemptOutcome, RuntimeVersionFacts,
    RuntimeVersionStream,
};
pub use schema::{
    McpContractError, McpContractLimitKind, McpInputSchema, McpOutputSchema, McpSchemaDialect,
    McpSingleSchemaKeyword, McpToolAnnotations,
};

use super::{
    AgentStackComponent, AgentStackComponentError, AgentStackComponentId, AgentStackComponentKind,
    AgentStackFreshness, AgentStackObservationClass, AgentStackSelectionState, AgentStackSource,
    AgentStackTrustLevel, Sha256Digest,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use thiserror::Error;

pub const AGENT_STACK_FINGERPRINT_SCHEMA_VERSION: &str = "agent-stack-fingerprint/v0.1";
pub const RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION: &str =
    "runtime-executable-fingerprint/v0.1";
pub const MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION: &str = "mcp-tool-fingerprint/v0.1";
pub const RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES: usize = 2_097_152;
pub const RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES: usize = 4_096;
pub const RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES: usize = 8_259;
pub const RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES: usize = 2_097_152;
pub const RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES: u64 = 67_108_864;

const RUNTIME_ROLE_NAMESPACE: &str = "harness_agent_runtime_role_v0_1";
const MCP_SERVER_NAMESPACE: &str = "harness_mcp_server_config_v0_1";
const MCP_TOOL_NAMESPACE: &str = "harness_mcp_tool_v0_1";

const FINGERPRINT_DIGEST_DOMAIN: &[u8] = b"harness_agent_stack_fingerprint_digest_v0_1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStackFingerprintSubject {
    AgentRuntime,
    McpTool,
}

impl AgentStackFingerprintSubject {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentRuntime => "agent_runtime",
            Self::McpTool => "mcp_tool",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFingerprintLimitKind {
    ExactSourceBytes,
    BaseSourceLocatorBytes,
    DerivedSourceLocatorBytes,
    EnvelopeBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredRuntimeSource {
    source: AgentStackSource,
    integrity: Option<Sha256Digest>,
}

impl ConfiguredRuntimeSource {
    pub fn without_canonical_bytes(
        source: AgentStackSource,
    ) -> Result<Self, AgentStackFingerprintError> {
        validate_base_source(&source)?;
        Ok(Self {
            source,
            integrity: None,
        })
    }

    pub fn from_exact_source_bytes(
        source: AgentStackSource,
        bytes: &[u8],
    ) -> Result<Self, AgentStackFingerprintError> {
        if bytes.len() > RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES {
            return Err(AgentStackFingerprintError::LimitExceeded(
                RuntimeFingerprintLimitKind::ExactSourceBytes,
            ));
        }
        validate_base_source(&source)?;
        Ok(Self {
            source,
            integrity: Some(Sha256Digest::from_bytes(bytes)),
        })
    }
    pub fn source(&self) -> &AgentStackSource {
        &self.source
    }
    pub fn integrity(&self) -> Option<&Sha256Digest> {
        self.integrity.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoleSourceBinding {
    base_source: AgentStackSource,
    source: AgentStackSource,
    runtime_kind: LocalExecutableRuntimeKind,
    integrity: Option<Sha256Digest>,
}

impl RuntimeRoleSourceBinding {
    pub fn derive(
        configured_source: &ConfiguredRuntimeSource,
        runtime_kind: LocalExecutableRuntimeKind,
    ) -> Result<Self, AgentStackFingerprintError> {
        let source = derive_source(
            configured_source.source(),
            RUNTIME_ROLE_NAMESPACE,
            runtime_kind.as_str().as_bytes(),
        )?;
        Ok(Self {
            base_source: configured_source.source().clone(),
            source,
            runtime_kind,
            integrity: configured_source.integrity().cloned(),
        })
    }

    fn parse(
        source: &AgentStackSource,
        runtime_kind: LocalExecutableRuntimeKind,
        integrity: Option<Sha256Digest>,
    ) -> Result<Self, AgentStackFingerprintError> {
        validate_derived_size(source)?;
        let (base_locator, decoded) =
            peel_suffix(source.locator().as_str(), RUNTIME_ROLE_NAMESPACE)?;
        validate_base_locator_size(base_locator)?;
        if decoded != runtime_kind.as_str().as_bytes() {
            return Err(AgentStackFingerprintError::InvalidComponentBinding);
        }
        let configured = ConfiguredRuntimeSource {
            source: AgentStackSource::new(source.scope(), base_locator)?,
            integrity,
        };
        let binding = Self::derive(&configured, runtime_kind)?;
        if binding.source != *source {
            return Err(AgentStackFingerprintError::InvalidComponentBinding);
        }
        Ok(binding)
    }
    pub fn base_source(&self) -> &AgentStackSource {
        &self.base_source
    }
    pub fn source(&self) -> &AgentStackSource {
        &self.source
    }
    pub const fn runtime_kind(&self) -> LocalExecutableRuntimeKind {
        self.runtime_kind
    }
    pub fn integrity(&self) -> Option<&Sha256Digest> {
        self.integrity.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredMcpServerBinding {
    base_source: AgentStackSource,
    server_source: AgentStackSource,
    stable_key: String,
    server_component_id: AgentStackComponentId,
}

impl ConfiguredMcpServerBinding {
    pub fn new(
        base_source: AgentStackSource,
        stable_key: &str,
    ) -> Result<Self, AgentStackFingerprintError> {
        validate_base_source(&base_source)?;
        if stable_key.len() > 1_024 {
            return Err(McpContractError::LimitExceeded(
                McpContractLimitKind::ConfiguredServerStableKeyBytes,
            )
            .into());
        }
        validate_nonblank(stable_key)?;
        let server_source =
            derive_source(&base_source, MCP_SERVER_NAMESPACE, stable_key.as_bytes())?;
        let server_component_id =
            AgentStackComponentId::from_source(AgentStackComponentKind::McpServer, &server_source);
        Ok(Self {
            base_source,
            server_source,
            stable_key: stable_key.to_owned(),
            server_component_id,
        })
    }
    pub fn base_source(&self) -> &AgentStackSource {
        &self.base_source
    }
    pub fn server_source(&self) -> &AgentStackSource {
        &self.server_source
    }
    pub fn stable_key(&self) -> &str {
        &self.stable_key
    }
    pub fn server_component_id(&self) -> &AgentStackComponentId {
        &self.server_component_id
    }
}

#[derive(Debug, Error)]
pub enum AgentStackFingerprintError {
    #[error("the Agent Stack fingerprint JSON has invalid syntax or shape")]
    Json(#[source] serde_json::Error),
    #[error(transparent)]
    Component(#[from] AgentStackComponentError),
    #[error(transparent)]
    McpContract(#[from] McpContractError),
    #[error("the fingerprint exceeds the {0:?} limit")]
    LimitExceeded(RuntimeFingerprintLimitKind),
    #[error("the fingerprint schema version is unsupported")]
    UnsupportedSchemaVersion,
    #[error("the fingerprint subject and payload do not agree")]
    SubjectPayloadMismatch,
    #[error("the fingerprint component does not match its subject or source binding")]
    InvalidComponentBinding,
    #[error("the fingerprint component must not declare capabilities")]
    NonEmptyCapabilities,
    #[error("the fingerprint component observation metadata is invalid")]
    InvalidObservationMetadata,
    #[error("the fingerprint payload contains an impossible state")]
    InvalidPayloadState,
    #[error("the fingerprint digest does not match the canonical payload")]
    FingerprintDigestMismatch,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStackFingerprintPayload {
    AgentRuntime(RuntimeExecutableFingerprintPayload),
    McpTool(McpToolFingerprintPayload),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStackFingerprintEnvelope {
    component: AgentStackComponent,
    payload: AgentStackFingerprintPayload,
    fingerprint_digest: Sha256Digest,
}

impl AgentStackFingerprintEnvelope {
    pub fn agent_runtime(
        payload: RuntimeExecutableFingerprintPayload,
    ) -> Result<Self, AgentStackFingerprintError> {
        payload.validate()?;
        let component = observed_component(
            AgentStackComponentKind::AgentRuntime,
            payload.role_binding().source().clone(),
            payload.role_binding().integrity().cloned(),
        )?;
        Self::finish(
            component,
            AgentStackFingerprintPayload::AgentRuntime(payload),
        )
    }

    pub fn mcp_tool(
        payload: McpToolFingerprintPayload,
    ) -> Result<Self, AgentStackFingerprintError> {
        payload.validate()?;
        let component = observed_component(
            AgentStackComponentKind::McpTool,
            payload.tool_source().clone(),
            None,
        )?;
        Self::finish(component, AgentStackFingerprintPayload::McpTool(payload))
    }

    fn finish(
        component: AgentStackComponent,
        payload: AgentStackFingerprintPayload,
    ) -> Result<Self, AgentStackFingerprintError> {
        let fingerprint_digest = digest_payload(&payload)?;
        Ok(Self {
            component,
            payload,
            fingerprint_digest,
        })
    }

    pub fn from_json_str(value: &str) -> Result<Self, AgentStackFingerprintError> {
        Self::from_json_slice(value.as_bytes())
    }

    pub fn from_json_slice(value: &[u8]) -> Result<Self, AgentStackFingerprintError> {
        if value.len() > RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES {
            return Err(AgentStackFingerprintError::LimitExceeded(
                RuntimeFingerprintLimitKind::EnvelopeBytes,
            ));
        }
        let header: EnvelopeHeader =
            serde_json::from_slice(value).map_err(AgentStackFingerprintError::Json)?;
        if header.schema_version != AGENT_STACK_FINGERPRINT_SCHEMA_VERSION {
            return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
        }
        let payload_header: PayloadVersionHeader =
            serde_json::from_str(header.payload.get()).map_err(AgentStackFingerprintError::Json)?;
        let payload_version = payload_header
            .schema_version
            .ok_or(AgentStackFingerprintError::UnsupportedSchemaVersion)?;
        match header.subject {
            AgentStackFingerprintSubject::AgentRuntime => {
                if payload_version == MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION {
                    return Err(AgentStackFingerprintError::SubjectPayloadMismatch);
                }
                if payload_version != RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION {
                    return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
                }
                parse_runtime_envelope(value)
            }
            AgentStackFingerprintSubject::McpTool => {
                if payload_version == RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION {
                    return Err(AgentStackFingerprintError::SubjectPayloadMismatch);
                }
                if payload_version != MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION {
                    return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
                }
                parse_mcp_envelope(value)
            }
        }
    }

    pub fn to_json_string(&self) -> Result<String, AgentStackFingerprintError> {
        match &self.payload {
            AgentStackFingerprintPayload::AgentRuntime(payload) => {
                let wire = RuntimeEnvelopeRef {
                    schema_version: AGENT_STACK_FINGERPRINT_SCHEMA_VERSION,
                    subject: AgentStackFingerprintSubject::AgentRuntime,
                    component: &self.component,
                    payload,
                    fingerprint_digest: &self.fingerprint_digest,
                };
                serde_json::to_string(&wire).map_err(AgentStackFingerprintError::Json)
            }
            AgentStackFingerprintPayload::McpTool(payload) => {
                let wire = McpEnvelopeRef {
                    schema_version: AGENT_STACK_FINGERPRINT_SCHEMA_VERSION,
                    subject: AgentStackFingerprintSubject::McpTool,
                    component: &self.component,
                    payload,
                    fingerprint_digest: &self.fingerprint_digest,
                };
                serde_json::to_string(&wire).map_err(AgentStackFingerprintError::Json)
            }
        }
    }

    pub const fn schema_version(&self) -> &'static str {
        AGENT_STACK_FINGERPRINT_SCHEMA_VERSION
    }
    pub fn subject(&self) -> AgentStackFingerprintSubject {
        match self.payload {
            AgentStackFingerprintPayload::AgentRuntime(_) => {
                AgentStackFingerprintSubject::AgentRuntime
            }
            AgentStackFingerprintPayload::McpTool(_) => AgentStackFingerprintSubject::McpTool,
        }
    }
    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }
    pub fn payload(&self) -> &AgentStackFingerprintPayload {
        &self.payload
    }
    pub fn fingerprint_digest(&self) -> &Sha256Digest {
        &self.fingerprint_digest
    }
}

fn observed_component(
    kind: AgentStackComponentKind,
    source: super::AgentStackSource,
    integrity: Option<Sha256Digest>,
) -> Result<AgentStackComponent, AgentStackFingerprintError> {
    Ok(AgentStackComponent::new(
        kind,
        source,
        AgentStackObservationClass::RunnerObserved,
        AgentStackSelectionState::Observed,
        AgentStackTrustLevel::RunnerObserved,
        AgentStackFreshness::Fresh,
    )?
    .with_integrity(integrity))
}

fn validate_component_common(
    component: &AgentStackComponent,
    expected_kind: AgentStackComponentKind,
) -> Result<(), AgentStackFingerprintError> {
    component.validate()?;
    if component.kind() != expected_kind {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    if !component.capabilities().is_empty() {
        return Err(AgentStackFingerprintError::NonEmptyCapabilities);
    }
    if component.observation_class() != AgentStackObservationClass::RunnerObserved
        || component.selection_state() != AgentStackSelectionState::Observed
        || component.trust_level() != AgentStackTrustLevel::RunnerObserved
        || component.freshness() != AgentStackFreshness::Fresh
    {
        return Err(AgentStackFingerprintError::InvalidObservationMetadata);
    }
    Ok(())
}

#[derive(Deserialize)]
struct EnvelopeHeader {
    schema_version: String,
    subject: AgentStackFingerprintSubject,
    payload: Box<RawValue>,
}

#[derive(Deserialize)]
struct PayloadVersionHeader {
    schema_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeEnvelopeWire<'a> {
    schema_version: String,
    subject: AgentStackFingerprintSubject,
    #[serde(borrow)]
    component: &'a RawValue,
    payload: model::RuntimePayloadWire,
    fingerprint_digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpEnvelopeWire<'a> {
    schema_version: String,
    subject: AgentStackFingerprintSubject,
    #[serde(borrow)]
    component: &'a RawValue,
    #[serde(borrow)]
    payload: model::McpPayloadWire<'a>,
    fingerprint_digest: String,
}

#[derive(Serialize)]
struct RuntimeEnvelopeRef<'a> {
    schema_version: &'static str,
    subject: AgentStackFingerprintSubject,
    component: &'a AgentStackComponent,
    payload: &'a RuntimeExecutableFingerprintPayload,
    fingerprint_digest: &'a Sha256Digest,
}

#[derive(Serialize)]
struct McpEnvelopeRef<'a> {
    schema_version: &'static str,
    subject: AgentStackFingerprintSubject,
    component: &'a AgentStackComponent,
    payload: &'a McpToolFingerprintPayload,
    fingerprint_digest: &'a Sha256Digest,
}

fn parse_runtime_envelope(
    value: &[u8],
) -> Result<AgentStackFingerprintEnvelope, AgentStackFingerprintError> {
    let wire: RuntimeEnvelopeWire =
        serde_json::from_slice(value).map_err(AgentStackFingerprintError::Json)?;
    if wire.schema_version != AGENT_STACK_FINGERPRINT_SCHEMA_VERSION {
        return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
    }
    if wire.subject != AgentStackFingerprintSubject::AgentRuntime {
        return Err(AgentStackFingerprintError::SubjectPayloadMismatch);
    }
    let component =
        AgentStackComponent::from_json(wire.component.get()).map_err(|error| match error {
            super::AgentStackComponentParseError::Syntax(error) => {
                AgentStackFingerprintError::Json(error)
            }
            super::AgentStackComponentParseError::Validation(error) => error.into(),
        })?;
    validate_component_common(&component, AgentStackComponentKind::AgentRuntime)?;
    let payload = RuntimeExecutableFingerprintPayload::from_wire(wire.payload, &component)?;
    let envelope = AgentStackFingerprintEnvelope {
        component,
        payload: AgentStackFingerprintPayload::AgentRuntime(payload),
        fingerprint_digest: Sha256Digest::parse(&wire.fingerprint_digest)?,
    };
    envelope.validate_digest()?;
    Ok(envelope)
}

fn parse_mcp_envelope(
    value: &[u8],
) -> Result<AgentStackFingerprintEnvelope, AgentStackFingerprintError> {
    let wire: McpEnvelopeWire =
        serde_json::from_slice(value).map_err(AgentStackFingerprintError::Json)?;
    if wire.schema_version != AGENT_STACK_FINGERPRINT_SCHEMA_VERSION {
        return Err(AgentStackFingerprintError::UnsupportedSchemaVersion);
    }
    if wire.subject != AgentStackFingerprintSubject::McpTool {
        return Err(AgentStackFingerprintError::SubjectPayloadMismatch);
    }
    let component =
        AgentStackComponent::from_json(wire.component.get()).map_err(|error| match error {
            super::AgentStackComponentParseError::Syntax(error) => {
                AgentStackFingerprintError::Json(error)
            }
            super::AgentStackComponentParseError::Validation(error) => error.into(),
        })?;
    validate_component_common(&component, AgentStackComponentKind::McpTool)?;
    if component.integrity().is_some() {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    let payload = McpToolFingerprintPayload::from_wire(wire.payload, &component)?;
    let envelope = AgentStackFingerprintEnvelope {
        component,
        payload: AgentStackFingerprintPayload::McpTool(payload),
        fingerprint_digest: Sha256Digest::parse(&wire.fingerprint_digest)?,
    };
    envelope.validate_digest()?;
    Ok(envelope)
}

impl AgentStackFingerprintEnvelope {
    fn validate_digest(&self) -> Result<(), AgentStackFingerprintError> {
        let expected = digest_payload(&self.payload)?;
        if expected == self.fingerprint_digest {
            Ok(())
        } else {
            Err(AgentStackFingerprintError::FingerprintDigestMismatch)
        }
    }
}

fn digest_payload(
    payload: &AgentStackFingerprintPayload,
) -> Result<Sha256Digest, AgentStackFingerprintError> {
    let (subject, version, canonical) = match payload {
        AgentStackFingerprintPayload::AgentRuntime(payload) => (
            AgentStackFingerprintSubject::AgentRuntime.as_str(),
            RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION,
            payload.canonical_bytes()?,
        ),
        AgentStackFingerprintPayload::McpTool(payload) => (
            AgentStackFingerprintSubject::McpTool.as_str(),
            MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION,
            payload.canonical_bytes()?,
        ),
    };
    fingerprint_digest_from_canonical_payload(subject, version, &canonical)
}

fn fingerprint_digest_from_canonical_payload(
    subject: &str,
    version: &str,
    payload: &[u8],
) -> Result<Sha256Digest, AgentStackFingerprintError> {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DIGEST_DOMAIN);
    update_frame(&mut hasher, subject.as_bytes())?;
    update_frame(&mut hasher, version.as_bytes())?;
    update_frame(&mut hasher, payload)?;
    let mut encoded = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(Sha256Digest::parse(&encoded)?)
}

fn update_frame(hasher: &mut Sha256, value: &[u8]) -> Result<(), AgentStackFingerprintError> {
    let length =
        u64::try_from(value.len()).map_err(|_| AgentStackFingerprintError::InvalidPayloadState)?;
    hasher.update(length.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn validate_base_source(source: &AgentStackSource) -> Result<(), AgentStackFingerprintError> {
    validate_base_locator_size(source.locator().as_str())?;
    AgentStackSource::new(source.scope(), source.locator().as_str())?;
    Ok(())
}

fn validate_base_locator_size(locator: &str) -> Result<(), AgentStackFingerprintError> {
    if locator.len() > RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES {
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::BaseSourceLocatorBytes,
        ))
    } else {
        Ok(())
    }
}

fn validate_derived_size(source: &AgentStackSource) -> Result<(), AgentStackFingerprintError> {
    if source.locator().as_str().len() > RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES {
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes,
        ))
    } else {
        Ok(())
    }
}

fn validate_nonblank(value: &str) -> Result<(), AgentStackFingerprintError> {
    if value.is_empty()
        || value
            .bytes()
            .all(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b' '))
    {
        Err(AgentStackFingerprintError::InvalidComponentBinding)
    } else {
        Ok(())
    }
}

fn derive_source(
    base: &AgentStackSource,
    namespace: &str,
    identity: &[u8],
) -> Result<AgentStackSource, AgentStackFingerprintError> {
    AgentStackSource::new(base.scope(), base.locator().as_str())?;
    let hex_len =
        identity
            .len()
            .checked_mul(2)
            .ok_or(AgentStackFingerprintError::LimitExceeded(
                RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes,
            ))?;
    let suffix = 1usize
        .checked_add(namespace.len())
        .and_then(|value| value.checked_add(3 + identity.len().to_string().len()))
        .and_then(|value| value.checked_add(hex_len))
        .ok_or(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes,
        ))?;
    let total = base.locator().as_str().len().checked_add(suffix).ok_or(
        AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes,
        ),
    )?;
    if total > RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES {
        return Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes,
        ));
    }
    let mut locator = String::with_capacity(total);
    locator.push_str(base.locator().as_str());
    locator.push('/');
    locator.push_str(namespace);
    locator.push_str("/u");
    locator.push_str(&identity.len().to_string());
    locator.push('_');
    for byte in identity {
        locator.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        locator.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    Ok(AgentStackSource::new(base.scope(), &locator)?)
}

pub(crate) fn derive_mcp_tool_source(
    server_source: &AgentStackSource,
    tool_name: &[u8],
) -> Result<AgentStackSource, AgentStackFingerprintError> {
    derive_source(server_source, MCP_TOOL_NAMESPACE, tool_name)
}

fn peel_suffix<'a>(
    locator: &'a str,
    namespace: &str,
) -> Result<(&'a str, Vec<u8>), AgentStackFingerprintError> {
    let (prefix, encoded) = locator
        .rsplit_once('/')
        .ok_or(AgentStackFingerprintError::InvalidComponentBinding)?;
    let (base, actual_namespace) = prefix
        .rsplit_once('/')
        .ok_or(AgentStackFingerprintError::InvalidComponentBinding)?;
    if actual_namespace != namespace {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    let (length, hex_value) = encoded
        .strip_prefix('u')
        .and_then(|value| value.split_once('_'))
        .ok_or(AgentStackFingerprintError::InvalidComponentBinding)?;
    if length.is_empty() || (length.len() > 1 && length.starts_with('0')) {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    let length: usize = length
        .parse()
        .map_err(|_| AgentStackFingerprintError::InvalidComponentBinding)?;
    let expected_hex_length = length
        .checked_mul(2)
        .ok_or(AgentStackFingerprintError::InvalidComponentBinding)?;
    if hex_value.len() != expected_hex_length {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    let mut decoded = Vec::with_capacity(length);
    for pair in hex_value.as_bytes().chunks_exact(2) {
        decoded.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Ok((base, decoded))
}

fn hex_nibble(value: u8) -> Result<u8, AgentStackFingerprintError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(AgentStackFingerprintError::InvalidComponentBinding),
    }
}

pub(crate) fn parse_mcp_tool_source(
    source: &AgentStackSource,
) -> Result<(AgentStackSource, String, String), AgentStackFingerprintError> {
    validate_derived_size(source)?;
    let (server_locator, tool_bytes) = peel_suffix(source.locator().as_str(), MCP_TOOL_NAMESPACE)?;
    let (base_locator, stable_key_bytes) = peel_suffix(server_locator, MCP_SERVER_NAMESPACE)?;
    validate_base_locator_size(base_locator)?;
    let stable_key = String::from_utf8(stable_key_bytes)
        .map_err(|_| AgentStackFingerprintError::InvalidComponentBinding)?;
    let tool_name = String::from_utf8(tool_bytes)
        .map_err(|_| AgentStackFingerprintError::InvalidComponentBinding)?;
    let base = AgentStackSource::new(source.scope(), base_locator)?;
    let binding = ConfiguredMcpServerBinding::new(base.clone(), &stable_key)?;
    if derive_mcp_tool_source(binding.server_source(), tool_name.as_bytes())? != *source {
        return Err(AgentStackFingerprintError::InvalidComponentBinding);
    }
    Ok((base, stable_key, tool_name))
}

#[cfg(test)]
pub(crate) fn digest_vector(subject: &str, version: &str, payload: &[u8]) -> Sha256Digest {
    fingerprint_digest_from_canonical_payload(subject, version, payload).unwrap()
}
