use super::*;
use crate::stack::{AgentStackSource, AgentStackSourceScope, Sha256Digest};

mod model;
mod schema;
mod validation;

#[test]
fn fingerprint_digest_framing_vectors_are_independent() {
    let payload = br#"{"a":1,"z":"\n"}"#;
    assert_eq!(
        digest_vector(
            "agent_runtime",
            RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION,
            payload
        )
        .as_str(),
        "3f45cc1b14c0099eaf056f9475aa210b4f84d45b2a4940ecff35079b3b1611fe"
    );
    assert_eq!(
        digest_vector("mcp_tool", MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION, payload).as_str(),
        "e00eca6b5f5a3fe3494cf590e68ec59f70e40ee54b7f7f42e48756d296fa85d9"
    );
}

fn runtime_payload(kind: LocalExecutableRuntimeKind) -> RuntimeExecutableFingerprintPayload {
    runtime_payload_with_observation(kind, None, None).unwrap()
}

fn runtime_payload_with_observation(
    kind: LocalExecutableRuntimeKind,
    executable: Option<RuntimeExecutableIdentity>,
    version: Option<RuntimeVersionFacts>,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    let (attempts, failures) = if version.is_some() {
        (
            vec![runtime_attempt(
                b"selected",
                RuntimeResolutionAttemptOutcome::ExecStarted,
                RuntimeExecSequence::Single,
            )],
            vec![],
        )
    } else {
        (
            vec![runtime_attempt(
                b"missing",
                RuntimeResolutionAttemptOutcome::Absent,
                RuntimeExecSequence::None,
            )],
            vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathNotFound).unwrap()],
        )
    };
    runtime_payload_with_facts(
        kind,
        RuntimeCommandForm::UnixBare,
        attempts,
        executable,
        version,
        failures,
    )
}

#[test]
fn unix_bare_path_not_found_requires_a_reached_attempt() {
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![],
        None,
        None,
        vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathNotFound).unwrap()],
    )
    .is_err());
}

fn runtime_payload_with_facts(
    kind: LocalExecutableRuntimeKind,
    command_form: RuntimeCommandForm,
    resolution_attempts: Vec<RuntimeResolutionAttempt>,
    executable: Option<RuntimeExecutableIdentity>,
    version: Option<RuntimeVersionFacts>,
    failures: Vec<RuntimeProbeFailure>,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    let base = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "configured_runtime",
        "primary",
    )
    .unwrap();
    let source = ConfiguredRuntimeSource::from_exact_source_bytes(base, b"runtime source").unwrap();
    runtime_payload_from_configured_source(
        source,
        kind,
        command_form,
        resolution_attempts,
        executable,
        version,
        failures,
    )
}

#[allow(clippy::too_many_arguments)]
fn runtime_payload_from_configured_source(
    source: ConfiguredRuntimeSource,
    kind: LocalExecutableRuntimeKind,
    command_form: RuntimeCommandForm,
    resolution_attempts: Vec<RuntimeResolutionAttempt>,
    executable: Option<RuntimeExecutableIdentity>,
    version: Option<RuntimeVersionFacts>,
    failures: Vec<RuntimeProbeFailure>,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    let binding = RuntimeRoleSourceBinding::derive(&source, kind).unwrap();
    let environment = match kind {
        LocalExecutableRuntimeKind::CodexExec | LocalExecutableRuntimeKind::CodexJsonrpc => vec![
            RuntimeEnvironmentFact::new(
                RuntimeEnvironmentKey::OpenaiApiKey,
                RuntimeEnvironmentValue::Unset,
            ),
            RuntimeEnvironmentFact::new(
                RuntimeEnvironmentKey::Path,
                RuntimeEnvironmentValue::SetDigest {
                    value_sha256: Sha256Digest::from_bytes(b"path"),
                },
            ),
        ],
        LocalExecutableRuntimeKind::ClaudeCode => vec![
            RuntimeEnvironmentFact::new(
                RuntimeEnvironmentKey::AnthropicApiKey,
                RuntimeEnvironmentValue::Unset,
            ),
            RuntimeEnvironmentFact::new(
                RuntimeEnvironmentKey::ClaudeConfigDir,
                RuntimeEnvironmentValue::Unset,
            ),
            RuntimeEnvironmentFact::new(
                RuntimeEnvironmentKey::Path,
                RuntimeEnvironmentValue::SetDigest {
                    value_sha256: Sha256Digest::from_bytes(b"path"),
                },
            ),
        ],
    };
    RuntimeExecutableFingerprintPayload::new(
        binding,
        command_form,
        Sha256Digest::from_bytes(b"command"),
        Sha256Digest::from_bytes(b"cwd"),
        Sha256Digest::from_bytes(b"cwd identity"),
        resolution_attempts,
        executable,
        version,
        environment,
        failures,
    )
}

fn runtime_attempt(
    candidate: &[u8],
    outcome: RuntimeResolutionAttemptOutcome,
    sequence: RuntimeExecSequence,
) -> RuntimeResolutionAttempt {
    RuntimeResolutionAttempt::new(
        Sha256Digest::from_bytes(candidate),
        outcome,
        sequence,
        (sequence != RuntimeExecSequence::None)
            .then_some(RuntimeExecutionContext::LinuxFdCloexecExecveatEmptyPathFd10),
    )
    .unwrap()
}

fn runtime_identity(
    checkpoint_consistent_path: bool,
    exec_stop_consistent_handle: bool,
) -> RuntimeExecutableIdentity {
    RuntimeExecutableIdentity::new(
        1,
        Some(0o100_755),
        Sha256Digest::from_bytes(b"executable"),
        checkpoint_consistent_path,
        exec_stop_consistent_handle,
    )
}

fn runtime_version() -> RuntimeVersionFacts {
    RuntimeVersionFacts::new(
        "1.2.3".to_owned(),
        Sha256Digest::from_bytes(b"codex-cli 1.2.3"),
        Sha256Digest::from_bytes(b""),
        RuntimeVersionStream::Stdout,
    )
    .unwrap()
}

fn mcp_payload(
    description: Option<&str>,
    annotations: Option<&str>,
    input_schema: &str,
    output_schema: Option<&str>,
) -> McpToolFingerprintPayload {
    let base =
        AgentStackSource::logical(AgentStackSourceScope::Runtime, "configured_mcp", "primary")
            .unwrap();
    McpToolFingerprintPayload::new(
        ConfiguredMcpServerBinding::new(base, "server A").unwrap(),
        "tool/One",
        description,
        annotations
            .map(McpToolAnnotations::from_json_str)
            .transpose()
            .unwrap(),
        McpInputSchema::from_json_str(input_schema).unwrap(),
        output_schema
            .map(McpOutputSchema::from_json_str)
            .transpose()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn envelope_round_trips_both_closed_subjects() {
    let runtime = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let runtime_json = runtime.to_json_string().unwrap();
    let parsed = AgentStackFingerprintEnvelope::from_json_str(&runtime_json).unwrap();
    assert_eq!(parsed.subject(), AgentStackFingerprintSubject::AgentRuntime);
    assert_eq!(parsed.fingerprint_digest(), runtime.fingerprint_digest());
    assert_eq!(
        parsed.component().integrity(),
        runtime.component().integrity()
    );

    let mcp = AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(
        Some("exact\n description"),
        Some(r#"{"readOnlyHint":true,"vendor":[2,1]}"#),
        r#"{"type":"object","required":["b","a"]}"#,
        Some(r#"{"type":"object"}"#),
    ))
    .unwrap();
    let mcp_json = mcp.to_json_string().unwrap();
    let parsed = AgentStackFingerprintEnvelope::from_json_str(&mcp_json).unwrap();
    assert_eq!(parsed.subject(), AgentStackFingerprintSubject::McpTool);
    assert_eq!(parsed.fingerprint_digest(), mcp.fingerprint_digest());
    assert!(parsed.component().integrity().is_none());
    assert!(parsed.component().capabilities().is_empty());
}

#[test]
fn envelope_rejects_version_subject_payload_capability_and_fingerprint_digest_mismatch() {
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let json = envelope.to_json_string().unwrap();
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(&json.replacen(
            AGENT_STACK_FINGERPRINT_SCHEMA_VERSION,
            "agent-stack-fingerprint/v9",
            1
        )),
        Err(AgentStackFingerprintError::UnsupportedSchemaVersion)
    ));
    assert!(AgentStackFingerprintEnvelope::from_json_str(&json.replacen(
        "agent_runtime",
        "mcp_tool",
        1
    ))
    .is_err());
    let capability_result = AgentStackFingerprintEnvelope::from_json_str(&json.replacen(
        "\"capabilities\":[]",
        "\"capabilities\":[\"network\"]",
        1,
    ));
    assert!(
        matches!(
            capability_result,
            Err(AgentStackFingerprintError::NonEmptyCapabilities)
        ),
        "{capability_result:?}"
    );
    let digest = envelope.fingerprint_digest().as_str();
    let replacement = if let Some(suffix) = digest.strip_prefix('a') {
        format!("b{suffix}")
    } else {
        format!("a{}", &digest[1..])
    };
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(&json.replacen(digest, &replacement, 1)),
        Err(AgentStackFingerprintError::FingerprintDigestMismatch)
    ));
}

#[test]
fn runtime_role_sources_are_pairwise_distinct_for_one_base() {
    let base =
        AgentStackSource::logical(AgentStackSourceScope::System, "runtime", "shared").unwrap();
    let configured = ConfiguredRuntimeSource::from_exact_source_bytes(base, b"same bytes").unwrap();
    let bindings = LocalExecutableRuntimeKind::ALL
        .iter()
        .map(|kind| RuntimeRoleSourceBinding::derive(&configured, *kind).unwrap())
        .collect::<Vec<_>>();
    assert_ne!(bindings[0].source(), bindings[1].source());
    assert_ne!(bindings[1].source(), bindings[2].source());
    assert!(bindings
        .iter()
        .all(|binding| binding.integrity() == configured.integrity()));
    assert!(bindings[0]
        .source()
        .locator()
        .as_str()
        .ends_with("/harness_agent_runtime_role_v0_1/u10_636f6465785f65786563"));
}

#[test]
fn configured_sources_enforce_exact_limits_before_derivation() {
    let base = AgentStackSource::logical(AgentStackSourceScope::Runner, "runtime", "base").unwrap();
    let too_many_bytes = vec![0; RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES + 1];
    assert!(matches!(
        ConfiguredRuntimeSource::from_exact_source_bytes(base.clone(), &too_many_bytes),
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::ExactSourceBytes
        ))
    ));
    assert!(ConfiguredMcpServerBinding::new(base.clone(), &"a".repeat(1_024)).is_ok());
    assert!(matches!(
        ConfiguredMcpServerBinding::new(base, &"a".repeat(1_025)),
        Err(AgentStackFingerprintError::McpContract(
            McpContractError::LimitExceeded(McpContractLimitKind::ConfiguredServerStableKeyBytes)
        ))
    ));
}

#[test]
fn runtime_fingerprint_limits_accept_exact_and_reject_limit_plus_one() {
    let base = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "configured_runtime",
        "exact-source",
    )
    .unwrap();
    assert!(ConfiguredRuntimeSource::from_exact_source_bytes(
        base.clone(),
        &vec![b'x'; RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES],
    )
    .is_ok());
    assert!(matches!(
        ConfiguredRuntimeSource::from_exact_source_bytes(
            base,
            &vec![b'x'; RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES + 1],
        ),
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::ExactSourceBytes
        ))
    ));

    let prefix = "configured_runtime/";
    let exact_locator = format!(
        "{prefix}{}",
        "a".repeat(RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES - prefix.len())
    );
    let exact_base = AgentStackSource::new(AgentStackSourceScope::Runner, &exact_locator).unwrap();
    assert!(ConfiguredRuntimeSource::without_canonical_bytes(exact_base).is_ok());
    let over_locator = format!("{exact_locator}a");
    let over_base = AgentStackSource::new(AgentStackSourceScope::Runner, &over_locator).unwrap();
    assert!(matches!(
        ConfiguredRuntimeSource::without_canonical_bytes(over_base),
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::BaseSourceLocatorBytes
        ))
    ));

    let envelope = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let json = envelope.to_json_string().unwrap();
    let exact_envelope = format!(
        "{json}{}",
        " ".repeat(RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES - json.len())
    );
    assert_eq!(exact_envelope.len(), RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES);
    assert!(AgentStackFingerprintEnvelope::from_json_str(&exact_envelope).is_ok());
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(&format!("{exact_envelope} ")),
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::EnvelopeBytes
        ))
    ));
}

#[test]
fn derived_source_parser_rejects_limit_plus_one_before_suffix_decoding() {
    let base = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "base",
        &"a".repeat(RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES - 5),
    )
    .unwrap();
    let payload = McpToolFingerprintPayload::new(
        ConfiguredMcpServerBinding::new(base, &"s".repeat(1_024)).unwrap(),
        &"t".repeat(1_024),
        None,
        None,
        McpInputSchema::from_json_str("{}").unwrap(),
        None,
    )
    .unwrap();
    let envelope = AgentStackFingerprintEnvelope::mcp_tool(payload).unwrap();
    let old_source = envelope.component().source();
    assert_eq!(
        old_source.locator().as_str().len(),
        RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES
    );
    let over_source = AgentStackSource::new(
        old_source.scope(),
        &format!("{}0", old_source.locator().as_str()),
    )
    .unwrap();
    let old_id = envelope.component().component_id();
    let over_id =
        AgentStackComponentId::from_source(AgentStackComponentKind::McpTool, &over_source);
    let invalid = envelope
        .to_json_string()
        .unwrap()
        .replacen(old_id.as_str(), over_id.as_str(), 1)
        .replacen(
            &format!("\"locator\":\"{}\"", old_source.locator().as_str()),
            &format!("\"locator\":\"{}\"", over_source.locator().as_str()),
            1,
        );
    let result = AgentStackFingerprintEnvelope::from_json_str(&invalid);
    assert!(
        matches!(
            result,
            Err(AgentStackFingerprintError::LimitExceeded(
                RuntimeFingerprintLimitKind::DerivedSourceLocatorBytes
            ))
        ),
        "{result:?}"
    );
}

#[test]
fn maximum_mcp_tool_locator_reaches_exact_derived_limit() {
    let base = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "base",
        &"a".repeat(RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES - 5),
    )
    .unwrap();
    assert_eq!(
        base.locator().as_str().len(),
        RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES
    );
    let binding = ConfiguredMcpServerBinding::new(base, &"s".repeat(1_024)).unwrap();
    let payload = McpToolFingerprintPayload::new(
        binding,
        &"t".repeat(1_024),
        None,
        None,
        McpInputSchema::from_json_str("{}").unwrap(),
        None,
    )
    .unwrap();
    let envelope = AgentStackFingerprintEnvelope::mcp_tool(payload).unwrap();
    assert_eq!(
        envelope.component().source().locator().as_str().len(),
        RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES
    );
}

#[test]
fn mcp_identity_uses_exact_ascii_blank_predicate_and_injective_sources() {
    let base = AgentStackSource::logical(AgentStackSourceScope::Runner, "mcp", "base").unwrap();
    for blank in ["", " ", "\t\r\n"] {
        assert!(ConfiguredMcpServerBinding::new(base.clone(), blank).is_err());
    }
    assert!(ConfiguredMcpServerBinding::new(base.clone(), "\u{000b}").is_ok());
    assert!(ConfiguredMcpServerBinding::new(base.clone(), "\u{00a0}").is_ok());
    let first =
        AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(None, None, r#"{}"#, None)).unwrap();
    let second = McpToolFingerprintPayload::new(
        ConfiguredMcpServerBinding::new(base, "server A").unwrap(),
        "tool/Two",
        None,
        None,
        McpInputSchema::from_json_str("{}").unwrap(),
        None,
    )
    .and_then(AgentStackFingerprintEnvelope::mcp_tool)
    .unwrap();
    assert_ne!(
        first.component().component_id(),
        second.component().component_id()
    );
}

#[test]
fn mcp_description_annotations_and_output_presence_remain_distinct() {
    let digest = |description, annotations, output| {
        AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(description, annotations, "{}", output))
            .unwrap()
            .fingerprint_digest()
            .as_str()
            .to_owned()
    };
    assert_ne!(digest(None, None, None), digest(Some(""), None, None));
    assert_ne!(
        digest(Some(" "), None, None),
        digest(Some("\t"), None, None)
    );
    assert_ne!(digest(None, None, None), digest(None, Some("{}"), None));
    assert_ne!(digest(None, None, None), digest(None, None, Some("{}")));
    assert_ne!(
        digest(None, Some(r#"{"vendor":[1,2]}"#), None),
        digest(None, Some(r#"{"vendor":[2,1]}"#), None)
    );
}

#[test]
fn schema_is_duplicate_aware_dialect_aware_and_context_aware() {
    assert!(matches!(
        McpInputSchema::from_json_str(r#"{"x":1,"x":2}"#),
        Err(McpContractError::DuplicateObjectKey(_))
    ));
    assert!(matches!(
        McpInputSchema::from_json_str("[]"),
        Err(McpContractError::RootNotObject)
    ));
    assert!(matches!(
        McpInputSchema::from_json_str(r#"{"$schema":"unknown"}"#),
        Err(McpContractError::UnsupportedSchemaDialect)
    ));
    assert!(matches!(
        McpInputSchema::from_json_str(
            r#"{"not":{"$schema":"https://json-schema.org/draft/2020-12/schema"}}"#
        ),
        Err(McpContractError::UnsupportedSchemaDialect)
    ));

    let digest = |schema: &str| {
        AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(None, None, schema, None))
            .unwrap()
            .fingerprint_digest()
            .as_str()
            .to_owned()
    };
    assert_eq!(
        digest(r#"{"required":["a","b"]}"#),
        digest(r#"{"required":["b","a"]}"#)
    );
    assert_ne!(
        digest(r#"{"default":{"required":["a","b"]}}"#),
        digest(r#"{"default":{"required":["b","a"]}}"#)
    );
    assert_ne!(
        digest(
            r#"{"$schema":"http://json-schema.org/draft-07/schema#","contentSchema":{"enum":[1,2]}}"#
        ),
        digest(
            r#"{"$schema":"http://json-schema.org/draft-07/schema#","contentSchema":{"enum":[2,1]}}"#
        )
    );
}

#[test]
fn canonical_payload_preserves_raw_json_number_tokens() {
    let digest = |number: &str| {
        let schema = format!(r#"{{"default":{number}}}"#);
        AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(None, None, &schema, None))
            .unwrap()
            .fingerprint_digest()
            .as_str()
            .to_owned()
    };
    assert_ne!(digest("1"), digest("1.0"));
    assert_ne!(digest("1.0"), digest("1e0"));
}

#[test]
fn schema_raw_limit_is_checked_before_parsing() {
    let exact = format!("{{}}{}", " ".repeat(1_048_574));
    assert!(McpInputSchema::from_json_str(&exact).is_ok());
    let over = format!("{exact} ");
    assert!(matches!(
        McpInputSchema::from_json_str(&over),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaRawBytes
        ))
    ));
}

#[test]
fn schema_and_annotation_depth_limits_are_exact() {
    fn nested(depth: usize) -> String {
        let mut value = "{}".to_owned();
        for _ in 1..depth {
            value = format!(r#"{{"x":{value}}}"#);
        }
        value
    }

    assert!(McpInputSchema::from_json_str(&nested(64)).is_ok());
    assert!(matches!(
        McpInputSchema::from_json_str(&nested(65)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaDepth
        ))
    ));
    assert!(McpToolAnnotations::from_json_str(&nested(32)).is_ok());
    assert!(matches!(
        McpToolAnnotations::from_json_str(&nested(33)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsDepth
        ))
    ));
}

#[test]
fn runtime_typed_constructors_reject_impossible_states() {
    assert!(RuntimeResolutionAttempt::new(
        Sha256Digest::from_bytes(b"candidate"),
        RuntimeResolutionAttemptOutcome::Absent,
        RuntimeExecSequence::None,
        Some(RuntimeExecutionContext::LinuxFdCloexecExecveatEmptyPathFd10),
    )
    .is_err());
    assert!(RuntimeProbeFailure::new(RuntimeProbeFailureKind::ProbeNotAuthorized).is_err());
    assert!(RuntimeProbeFailure::with_detail(
        RuntimeProbeFailureKind::ProbeNotAuthorized,
        RuntimeProbeFailureDetail::ConfigurationSourceRepository,
    )
    .is_ok());
    assert!(RuntimeProbeFailure::with_detail(
        RuntimeProbeFailureKind::NonzeroExit,
        RuntimeProbeFailureDetail::KernelCodeLoading,
    )
    .is_err());
}

#[test]
fn closed_failure_vocabulary_and_canonical_ordering_are_enforced() {
    let cleanup = vec![
        RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
        RuntimeProbeFailure::new(RuntimeProbeFailureKind::TerminationFailed).unwrap(),
        RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
    ];
    let payload = runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"selected",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        None,
        None,
        cleanup.clone(),
    )
    .unwrap();
    let json = AgentStackFingerprintEnvelope::agent_runtime(payload)
        .unwrap()
        .to_json_string()
        .unwrap();
    assert_eq!(json.matches("\"phase\":\"lifecycle_cleanup\"").count(), 2);

    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"selected",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        None,
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
        ],
    )
    .is_ok());

    let mut reversed = cleanup;
    reversed.reverse();
    assert!(matches!(
        runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![runtime_attempt(
                b"selected",
                RuntimeResolutionAttemptOutcome::ExecStarted,
                RuntimeExecSequence::Single,
            )],
            None,
            None,
            reversed,
        ),
        Err(AgentStackFingerprintError::InvalidPayloadState)
    ));

    assert!(RuntimeProbeFailure::with_detail(
        RuntimeProbeFailureKind::TransitiveExecutionDenied,
        RuntimeProbeFailureDetail::KernelCodeLoading,
    )
    .is_ok());
}

#[test]
fn runtime_version_requires_both_identity_consistency_proofs() {
    for (checkpoint_consistent_path, exec_stop_consistent_handle) in [(false, true), (true, false)]
    {
        assert!(matches!(
            runtime_payload_with_observation(
                LocalExecutableRuntimeKind::CodexExec,
                Some(runtime_identity(
                    checkpoint_consistent_path,
                    exec_stop_consistent_handle,
                )),
                Some(runtime_version()),
            ),
            Err(AgentStackFingerprintError::InvalidPayloadState)
        ));
    }
}

#[test]
fn runtime_parser_rejects_version_without_both_identity_consistency_proofs() {
    let payload = runtime_payload_with_observation(
        LocalExecutableRuntimeKind::CodexExec,
        Some(runtime_identity(true, true)),
        Some(runtime_version()),
    )
    .unwrap();
    let json = AgentStackFingerprintEnvelope::agent_runtime(payload)
        .unwrap()
        .to_json_string()
        .unwrap();

    for field in ["checkpoint_consistent_path", "exec_stop_consistent_handle"] {
        let invalid = json.replacen(
            &format!(r#""{field}":true"#),
            &format!(r#""{field}":false"#),
            1,
        );
        assert_ne!(invalid, json, "fixture did not contain {field}");
        assert!(matches!(
            AgentStackFingerprintEnvelope::from_json_str(&invalid),
            Err(AgentStackFingerprintError::InvalidPayloadState)
        ));
    }
}

#[test]
fn envelope_raw_limit_precedes_json_decoding() {
    let value = vec![b' '; RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES + 1];
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_slice(&value),
        Err(AgentStackFingerprintError::LimitExceeded(
            RuntimeFingerprintLimitKind::EnvelopeBytes
        ))
    ));
}
