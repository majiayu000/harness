use super::*;

fn failure(kind: RuntimeProbeFailureKind) -> Vec<RuntimeProbeFailure> {
    vec![RuntimeProbeFailure::new(kind).unwrap()]
}

#[test]
fn runtime_attempt_sequence_and_outcome_matrix_is_closed() {
    assert!(RuntimeResolutionAttempt::new(
        Sha256Digest::from_bytes(b"candidate"),
        RuntimeResolutionAttemptOutcome::ExecStarted,
        RuntimeExecSequence::None,
        None,
    )
    .is_err());
    let terminal_then_skip = runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![
            runtime_attempt(
                b"terminal",
                RuntimeResolutionAttemptOutcome::InspectionFailed,
                RuntimeExecSequence::None,
            ),
            runtime_attempt(
                b"later",
                RuntimeResolutionAttemptOutcome::Absent,
                RuntimeExecSequence::None,
            ),
        ],
        None,
        None,
        failure(RuntimeProbeFailureKind::OpenFailed),
    );
    assert!(matches!(
        terminal_then_skip,
        Err(AgentStackFingerprintError::InvalidPayloadState)
    ));
}

#[test]
fn pre_spawn_identity_change_is_a_terminal_inspection_failure() {
    let payload = runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixAbsolute,
        vec![runtime_attempt(
            b"changed-before-spawn",
            RuntimeResolutionAttemptOutcome::InspectionFailed,
            RuntimeExecSequence::None,
        )],
        None,
        None,
        failure(RuntimeProbeFailureKind::IdentityChanged),
    );
    assert!(payload.is_ok());
}

#[test]
fn absolute_and_qualified_forms_require_exactly_one_non_fallback_attempt() {
    for form in [
        RuntimeCommandForm::UnixAbsolute,
        RuntimeCommandForm::UnixQualified,
    ] {
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            form,
            vec![runtime_attempt(
                b"missing",
                RuntimeResolutionAttemptOutcome::Absent,
                RuntimeExecSequence::None,
            )],
            None,
            None,
            failure(RuntimeProbeFailureKind::PathNotFound),
        )
        .is_ok());
        for attempts in [
            vec![],
            vec![
                runtime_attempt(
                    b"first",
                    RuntimeResolutionAttemptOutcome::Absent,
                    RuntimeExecSequence::None,
                ),
                runtime_attempt(
                    b"second",
                    RuntimeResolutionAttemptOutcome::Absent,
                    RuntimeExecSequence::None,
                ),
            ],
            vec![runtime_attempt(
                b"eacces",
                RuntimeResolutionAttemptOutcome::ExecEacces,
                RuntimeExecSequence::Single,
            )],
        ] {
            assert!(runtime_payload_with_facts(
                LocalExecutableRuntimeKind::CodexExec,
                form,
                attempts,
                None,
                None,
                failure(RuntimeProbeFailureKind::PathNotFound),
            )
            .is_err());
        }
    }
}

#[test]
fn candidate_limit_requires_exactly_64_nonterminal_bare_attempts() {
    let attempts = (0..64)
        .map(|index| {
            runtime_attempt(
                format!("candidate-{index}").as_bytes(),
                RuntimeResolutionAttemptOutcome::Absent,
                RuntimeExecSequence::None,
            )
        })
        .collect::<Vec<_>>();
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts.clone(),
        None,
        None,
        failure(RuntimeProbeFailureKind::CandidateLimitExceeded),
    )
    .is_ok());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts[..63].to_vec(),
        None,
        None,
        failure(RuntimeProbeFailureKind::CandidateLimitExceeded),
    )
    .is_err());
}

#[test]
fn failure_requires_one_matching_primary_and_optional_ordered_cleanup() {
    let started = vec![runtime_attempt(
        b"selected",
        RuntimeResolutionAttemptOutcome::ExecStarted,
        RuntimeExecSequence::Single,
    )];
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        started.clone(),
        Some(runtime_identity(true, true)),
        None,
        failure(RuntimeProbeFailureKind::Timeout),
    )
    .is_ok());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        started.clone(),
        Some(runtime_identity(true, true)),
        None,
        failure(RuntimeProbeFailureKind::SpawnFailed),
    )
    .is_err());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        started,
        Some(runtime_identity(true, true)),
        None,
        vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap()],
    )
    .is_err());
}

#[test]
fn every_post_exit_classification_is_a_version_probe_primary_failure() {
    for kind in [
        RuntimeProbeFailureKind::TerminatedBySignal,
        RuntimeProbeFailureKind::InvalidUtf8,
        RuntimeProbeFailureKind::EmptyOutput,
        RuntimeProbeFailureKind::UnparseableVersion,
        RuntimeProbeFailureKind::AmbiguousVersion,
    ] {
        let result = runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![runtime_attempt(
                b"selected",
                RuntimeResolutionAttemptOutcome::ExecStarted,
                RuntimeExecSequence::Single,
            )],
            Some(runtime_identity(true, true)),
            None,
            failure(kind),
        );
        assert!(
            result.is_ok(),
            "{kind:?} was not a primary failure: {result:?}"
        );
    }
}

#[test]
fn version_success_requires_started_attempt_stable_identity_and_no_failures() {
    assert!(runtime_payload_with_observation(
        LocalExecutableRuntimeKind::CodexExec,
        Some(runtime_identity(true, true)),
        Some(runtime_version()),
    )
    .is_ok());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"selected",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        Some(runtime_identity(true, true)),
        Some(runtime_version()),
        failure(RuntimeProbeFailureKind::Timeout),
    )
    .is_err());
}

#[test]
fn normalized_runtime_version_is_strict_semver() {
    for valid in ["0.0.0", "1.2.3", "1.2.3-alpha.1", "1.2.3+BUILD.01"] {
        assert!(RuntimeVersionFacts::new(
            valid.to_owned(),
            Sha256Digest::from_bytes(b"stdout"),
            Sha256Digest::from_bytes(b"stderr"),
            RuntimeVersionStream::Stdout,
        )
        .is_ok());
    }
    for invalid in [
        "",
        "v1.2.3",
        "1.2",
        "01.2.3",
        "1.02.3",
        "1.2.03",
        "1.2.3-01",
        "1.2.3-",
        "1.2.3+",
        "1.2.3+x+y",
        "1.2.3\n",
    ] {
        assert!(
            RuntimeVersionFacts::new(
                invalid.to_owned(),
                Sha256Digest::from_bytes(b"stdout"),
                Sha256Digest::from_bytes(b"stderr"),
                RuntimeVersionStream::Stdout,
            )
            .is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn runtime_environment_requires_the_exact_kind_policy_key_set() {
    let base = runtime_payload(LocalExecutableRuntimeKind::CodexExec);
    assert_eq!(base.runtime_kind(), LocalExecutableRuntimeKind::CodexExec);
    let claude_environment = vec![
        RuntimeEnvironmentFact::new(
            RuntimeEnvironmentKey::AnthropicApiKey,
            RuntimeEnvironmentValue::Unset,
        ),
        RuntimeEnvironmentFact::new(
            RuntimeEnvironmentKey::ClaudeConfigDir,
            RuntimeEnvironmentValue::Unset,
        ),
        RuntimeEnvironmentFact::new(RuntimeEnvironmentKey::Path, RuntimeEnvironmentValue::Unset),
    ];
    let source = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "configured_runtime",
        "claude",
    )
    .unwrap();
    let configured = ConfiguredRuntimeSource::without_canonical_bytes(source).unwrap();
    assert!(RuntimeExecutableFingerprintPayload::new(
        RuntimeRoleSourceBinding::derive(&configured, LocalExecutableRuntimeKind::ClaudeCode)
            .unwrap(),
        RuntimeCommandForm::UnixBare,
        Sha256Digest::from_bytes(b"command"),
        Sha256Digest::from_bytes(b"cwd"),
        Sha256Digest::from_bytes(b"cwd identity"),
        vec![],
        None,
        None,
        claude_environment,
        failure(RuntimeProbeFailureKind::PathUnusable),
    )
    .is_ok());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![],
        None,
        None,
        failure(RuntimeProbeFailureKind::PathUnusable),
    )
    .is_ok());
}

#[test]
fn runtime_environment_rejects_unknown_and_state_incompatible_fields() {
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let json = envelope.to_json_string().unwrap();
    for invalid in [
        json.replacen(
            "\"state\":\"unset\"",
            "\"state\":\"unset\",\"extra\":true",
            1,
        ),
        json.replacen(
            "\"state\":\"unset\"",
            &format!(
                "\"state\":\"unset\",\"value_sha256\":\"{}\"",
                Sha256Digest::from_bytes(b"forbidden").as_str()
            ),
            1,
        ),
    ] {
        assert_ne!(invalid, json);
        assert!(AgentStackFingerprintEnvelope::from_json_str(&invalid).is_err());
    }
}

#[test]
fn failure_payload_changes_fingerprint_digest_without_fabricating_integrity() {
    let path_not_found = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let path_unusable = AgentStackFingerprintEnvelope::agent_runtime(
        runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![],
            None,
            None,
            failure(RuntimeProbeFailureKind::PathUnusable),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(
        path_not_found.fingerprint_digest(),
        path_unusable.fingerprint_digest()
    );
    assert_eq!(
        path_not_found.component().integrity(),
        path_unusable.component().integrity()
    );
}

fn configured_source(
    scope: AgentStackSourceScope,
    identity: &str,
    exact_bytes: Option<&[u8]>,
) -> ConfiguredRuntimeSource {
    let source = base_source(scope, "configured_runtime", identity);
    match exact_bytes {
        Some(bytes) => ConfiguredRuntimeSource::from_exact_source_bytes(source, bytes).unwrap(),
        None => ConfiguredRuntimeSource::without_canonical_bytes(source).unwrap(),
    }
}

fn base_source(scope: AgentStackSourceScope, namespace: &str, identity: &str) -> AgentStackSource {
    match scope {
        AgentStackSourceScope::Repository | AgentStackSourceScope::Admin => {
            AgentStackSource::new(scope, &format!("{namespace}/{identity}")).unwrap()
        }
        AgentStackSourceScope::UserGlobal => {
            AgentStackSource::new(scope, &format!("home_harness/{namespace}/{identity}")).unwrap()
        }
        AgentStackSourceScope::System
        | AgentStackSourceScope::Runtime
        | AgentStackSourceScope::Runner => {
            AgentStackSource::logical(scope, namespace, identity).unwrap()
        }
    }
}

fn failed_runtime_from_source(
    source: ConfiguredRuntimeSource,
) -> RuntimeExecutableFingerprintPayload {
    runtime_payload_from_configured_source(
        source,
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![],
        None,
        None,
        failure(RuntimeProbeFailureKind::PathUnusable),
    )
    .unwrap()
}

fn json_with_component_source(
    envelope: &AgentStackFingerprintEnvelope,
    source: &AgentStackSource,
    kind: AgentStackComponentKind,
) -> String {
    let old_source = envelope.component().source();
    let old_id = envelope.component().component_id();
    let new_id = AgentStackComponentId::from_source(kind, source);
    envelope
        .to_json_string()
        .unwrap()
        .replacen(old_id.as_str(), new_id.as_str(), 1)
        .replacen(old_source.locator().as_str(), source.locator().as_str(), 1)
}

#[test]
fn fingerprint_digest_is_separate_from_component_integrity() {
    let first = AgentStackFingerprintEnvelope::agent_runtime(failed_runtime_from_source(
        configured_source(AgentStackSourceScope::Runner, "same", Some(b"first bytes")),
    ))
    .unwrap();
    let second = AgentStackFingerprintEnvelope::agent_runtime(failed_runtime_from_source(
        configured_source(AgentStackSourceScope::Runner, "same", Some(b"second bytes")),
    ))
    .unwrap();
    assert_ne!(
        first.component().integrity(),
        second.component().integrity()
    );
    assert_eq!(first.fingerprint_digest(), second.fingerprint_digest());
}

#[test]
fn component_integrity_preserves_exact_source_bytes_or_absence() {
    let exact = AgentStackFingerprintEnvelope::agent_runtime(failed_runtime_from_source(
        configured_source(
            AgentStackSourceScope::System,
            "exact",
            Some(b"exact source bytes"),
        ),
    ))
    .unwrap();
    assert_eq!(
        exact.component().integrity(),
        Some(&Sha256Digest::from_bytes(b"exact source bytes"))
    );
    let absent = AgentStackFingerprintEnvelope::agent_runtime(failed_runtime_from_source(
        configured_source(AgentStackSourceScope::System, "absent", None),
    ))
    .unwrap();
    assert!(absent.component().integrity().is_none());
}

#[test]
fn runtime_role_source_preserves_scope_and_exact_source_integrity_or_absence() {
    for scope in AgentStackSourceScope::ALL.iter().copied() {
        for exact in [None, Some(b"source".as_slice())] {
            let configured = configured_source(scope, "scope", exact);
            let binding = RuntimeRoleSourceBinding::derive(
                &configured,
                LocalExecutableRuntimeKind::CodexExec,
            )
            .unwrap();
            assert_eq!(binding.source().scope(), scope);
            assert_eq!(binding.integrity(), configured.integrity());
        }
    }
}

#[test]
fn caller_cannot_preencode_or_override_runtime_role_source() {
    let preencoded = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "configured_runtime",
        "base/harness_agent_runtime_role_v0_1/u10_636f6465785f65786563",
    )
    .unwrap();
    let configured = ConfiguredRuntimeSource::without_canonical_bytes(preencoded.clone()).unwrap();
    let binding =
        RuntimeRoleSourceBinding::derive(&configured, LocalExecutableRuntimeKind::CodexExec)
            .unwrap();
    assert_eq!(binding.base_source(), &preencoded);
    assert_ne!(binding.source(), &preencoded);
    assert!(binding
        .source()
        .locator()
        .as_str()
        .ends_with("/harness_agent_runtime_role_v0_1/u10_636f6465785f65786563"));
}

#[test]
fn runtime_role_parser_rejects_missing_malformed_noncanonical_and_wrong_role_suffixes() {
    let configured = configured_source(AgentStackSourceScope::Runner, "parser", None);
    let payload = failed_runtime_from_source(configured.clone());
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(payload).unwrap();
    let valid =
        RuntimeRoleSourceBinding::derive(&configured, LocalExecutableRuntimeKind::CodexExec)
            .unwrap();
    let jsonrpc =
        RuntimeRoleSourceBinding::derive(&configured, LocalExecutableRuntimeKind::CodexJsonrpc)
            .unwrap();
    let valid_locator = valid.source().locator().as_str();
    let malformed = [
        configured.source().clone(),
        AgentStackSource::new(
            valid.source().scope(),
            &valid_locator.replacen("/u10_", "/u010_", 1),
        )
        .unwrap(),
        AgentStackSource::new(
            valid.source().scope(),
            &valid_locator.replacen("636f", "636F", 1),
        )
        .unwrap(),
        jsonrpc.source().clone(),
    ];
    for source in malformed {
        let json =
            json_with_component_source(&envelope, &source, AgentStackComponentKind::AgentRuntime);
        let result = AgentStackFingerprintEnvelope::from_json_str(&json);
        assert!(
            matches!(
                result,
                Err(AgentStackFingerprintError::InvalidComponentBinding)
            ),
            "{result:?}"
        );
    }
}

fn mcp_binding(scope: AgentStackSourceScope, stable_key: &str) -> ConfiguredMcpServerBinding {
    ConfiguredMcpServerBinding::new(base_source(scope, "configured_mcp", "base"), stable_key)
        .unwrap()
}

fn mcp_from_binding(
    binding: ConfiguredMcpServerBinding,
    tool_name: &str,
) -> McpToolFingerprintPayload {
    McpToolFingerprintPayload::new(
        binding,
        tool_name,
        None,
        None,
        McpInputSchema::from_json_str("{}").unwrap(),
        None,
    )
    .unwrap()
}

#[test]
fn configured_mcp_server_binding_uses_exact_stable_key() {
    let binding = mcp_binding(AgentStackSourceScope::Runtime, " server\u{000b}A ");
    assert_eq!(binding.stable_key(), " server\u{000b}A ");
    assert!(binding
        .server_source()
        .locator()
        .as_str()
        .ends_with("/harness_mcp_server_config_v0_1/u10_207365727665720b4120"));
}

#[test]
fn configured_mcp_server_key_accepts_1024_and_rejects_1025_before_expansion() {
    let base =
        AgentStackSource::logical(AgentStackSourceScope::Runner, "configured_mcp", "key-limit")
            .unwrap();
    assert!(ConfiguredMcpServerBinding::new(base.clone(), &"s".repeat(1_024)).is_ok());
    assert!(matches!(
        ConfiguredMcpServerBinding::new(base, &"s".repeat(1_025)),
        Err(AgentStackFingerprintError::McpContract(
            McpContractError::LimitExceeded(McpContractLimitKind::ConfiguredServerStableKeyBytes)
        ))
    ));
}

#[test]
fn distinct_mcp_server_keys_have_distinct_ids() {
    let first = mcp_binding(AgentStackSourceScope::Runner, "server-a");
    let second = mcp_binding(AgentStackSourceScope::Runner, "server-b");
    assert_ne!(first.server_component_id(), second.server_component_id());
    assert_ne!(first.server_source(), second.server_source());
}

#[test]
fn mcp_tool_source_is_injective_for_multiple_tools_on_one_server() {
    let binding = mcp_binding(AgentStackSourceScope::Runtime, "server");
    let first = mcp_from_binding(binding.clone(), "tool-a");
    let second = mcp_from_binding(binding, "tool-b");
    assert_ne!(first.tool_source(), second.tool_source());
}

#[test]
fn mcp_tool_source_preserves_scope_and_encodes_exact_utf8_identity() {
    let payload = mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Repository, "server"),
        "工具/ß",
    );
    assert_eq!(
        payload.tool_source().scope(),
        AgentStackSourceScope::Repository
    );
    assert!(payload
        .tool_source()
        .locator()
        .as_str()
        .ends_with("/harness_mcp_tool_v0_1/u9_e5b7a5e585b72fc39f"));
}

#[test]
fn mcp_server_and_tool_suffix_mismatches_are_rejected() {
    let first_binding = mcp_binding(AgentStackSourceScope::Runtime, "server-a");
    let payload = mcp_from_binding(first_binding, "tool-a");
    let envelope = AgentStackFingerprintEnvelope::mcp_tool(payload).unwrap();
    let mismatched_tool = mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Runtime, "server-a"),
        "tool-b",
    );
    let mismatched_server = mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Runtime, "server-b"),
        "tool-a",
    );
    for source in [
        mismatched_tool.tool_source(),
        mismatched_server.tool_source(),
    ] {
        let json = json_with_component_source(&envelope, source, AgentStackComponentKind::McpTool);
        let result = AgentStackFingerprintEnvelope::from_json_str(&json);
        assert!(
            matches!(
                result,
                Err(AgentStackFingerprintError::InvalidComponentBinding)
            ),
            "{result:?}"
        );
    }
}

#[test]
fn arbitrary_mcp_server_component_is_not_accepted() {
    let envelope = AgentStackFingerprintEnvelope::mcp_tool(mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Runtime, "server"),
        "tool",
    ))
    .unwrap();
    let json = json_with_component_source(
        &envelope,
        envelope.component().source(),
        AgentStackComponentKind::McpServer,
    )
    .replacen("\"kind\":\"mcp_tool\"", "\"kind\":\"mcp_server\"", 1);
    assert!(AgentStackFingerprintEnvelope::from_json_str(&json).is_err());
}

#[test]
fn caller_cannot_supply_preencoded_mcp_tool_source() {
    let preencoded_name = "base/harness_mcp_tool_v0_1/u4_746f6f6c";
    let payload = mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Runner, "server"),
        preencoded_name,
    );
    assert_eq!(payload.tool_name(), preencoded_name);
    assert!(payload.tool_source().locator().as_str().ends_with(&format!(
        "/harness_mcp_tool_v0_1/u{}_{}",
        preencoded_name.len(),
        preencoded_name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )));
}

#[test]
fn runner_observation_preserves_every_runtime_and_mcp_source_identity() {
    let mut runtime_sources = std::collections::HashSet::new();
    for kind in LocalExecutableRuntimeKind::ALL {
        let envelope =
            AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(*kind)).unwrap();
        assert_eq!(
            envelope.component().source().scope(),
            AgentStackSourceScope::Runner
        );
        assert!(envelope
            .component()
            .source()
            .locator()
            .as_str()
            .contains("harness_agent_runtime_role_v0_1"));
        assert!(runtime_sources.insert(envelope.component().source().locator().as_str().to_owned()));
    }
    let mcp = AgentStackFingerprintEnvelope::mcp_tool(mcp_from_binding(
        mcp_binding(AgentStackSourceScope::Runner, "server"),
        "tool",
    ))
    .unwrap();
    assert_eq!(
        mcp.component().source().scope(),
        AgentStackSourceScope::Runner
    );
    assert!(mcp
        .component()
        .source()
        .locator()
        .as_str()
        .contains("harness_mcp_server_config_v0_1"));
    assert!(mcp
        .component()
        .source()
        .locator()
        .as_str()
        .contains("harness_mcp_tool_v0_1"));
}

#[test]
fn canonical_payload_string_escaping_is_frozen() {
    let annotations = McpToolAnnotations::from_json_str(r#"{"x":"\"\\\b\t\n\f\r\u0000"}"#).unwrap();
    assert_eq!(
        annotations.canonical_bytes(),
        br#"{"x":"\"\\\b\t\n\f\r\u0000"}"#
    );
}

#[test]
fn complete_runtime_and_mcp_payload_digest_vectors_are_fixed() {
    let runtime = AgentStackFingerprintEnvelope::agent_runtime(
        runtime_payload_with_observation(
            LocalExecutableRuntimeKind::CodexExec,
            Some(runtime_identity(true, true)),
            Some(runtime_version()),
        )
        .unwrap(),
    )
    .unwrap();
    let mcp = AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(
        Some("exact\n description"),
        Some(r#"{"readOnlyHint":true,"vendor":[2,1]}"#),
        r#"{"type":"object","required":["b","a"]}"#,
        Some(r#"{"type":"object"}"#),
    ))
    .unwrap();
    assert_eq!(
        runtime.fingerprint_digest().as_str(),
        "790a4d64d91542aa9f808fffb0f18ad599b853eeae7ba476fb6ce940dd6f2fa5"
    );
    assert_eq!(
        mcp.fingerprint_digest().as_str(),
        "99cdc42523cf6c65e55ba248051900f2b936b17403c0d66b337fc1814dce126e"
    );
}

#[test]
fn runtime_optional_fields_reject_explicit_null() {
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(runtime_payload(
        LocalExecutableRuntimeKind::CodexExec,
    ))
    .unwrap();
    let json = envelope.to_json_string().unwrap();
    for invalid in [
        json.replacen(
            "\"environment\":",
            "\"executable\":null,\"environment\":",
            1,
        ),
        json.replacen("\"environment\":", "\"version\":null,\"environment\":", 1),
        json.replacen(
            "\"kind\":\"path_not_found\"",
            "\"kind\":\"path_not_found\",\"detail\":null",
            1,
        ),
    ] {
        assert_ne!(invalid, json);
        assert!(AgentStackFingerprintEnvelope::from_json_str(&invalid).is_err());
    }
}

#[test]
fn mcp_optional_fields_reject_explicit_null() {
    let envelope =
        AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(None, None, "{}", None)).unwrap();
    let json = envelope.to_json_string().unwrap();
    for field in ["description", "annotations", "outputSchema"] {
        let invalid = json.replacen(
            "\"inputSchema\":",
            &format!("\"{field}\":null,\"inputSchema\":"),
            1,
        );
        assert_ne!(invalid, json);
        assert!(AgentStackFingerprintEnvelope::from_json_str(&invalid).is_err());
    }
}
