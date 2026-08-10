use super::*;

fn inspection_payload(
    identity: Option<RuntimeExecutableIdentity>,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"inspection",
            RuntimeResolutionAttemptOutcome::InspectionTarget,
            RuntimeExecSequence::None,
        )],
        identity,
        None,
        vec![RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::ProbeNotAuthorized,
            RuntimeProbeFailureDetail::ResolvedTargetRepository,
        )
        .unwrap()],
    )
}

fn failed_execution_payload(
    identity: Option<RuntimeExecutableIdentity>,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"executed",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        identity,
        None,
        vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::InvalidUtf8).unwrap()],
    )
}

fn successful_payload(
    kind: LocalExecutableRuntimeKind,
    version: RuntimeVersionFacts,
) -> Result<RuntimeExecutableFingerprintPayload, AgentStackFingerprintError> {
    runtime_payload_with_facts(
        kind,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"executed",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        Some(runtime_identity(true, true)),
        Some(version),
        Vec::new(),
    )
}

fn assert_parser_rejects_identity(
    payload: RuntimeExecutableFingerprintPayload,
    identity: Option<RuntimeExecutableIdentity>,
) {
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(payload).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    let payload = json["payload"].as_object_mut().unwrap();
    match identity {
        Some(identity) => {
            payload.insert(
                "executable".to_owned(),
                serde_json::to_value(identity).unwrap(),
            );
        }
        None => {
            payload.remove("executable");
        }
    }
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(&serde_json::to_string(&json).unwrap()),
        Err(AgentStackFingerprintError::InvalidPayloadState)
    ));
}

#[test]
fn inspection_target_requires_the_nonexecuted_identity_shape() {
    let valid = inspection_payload(Some(runtime_identity(false, false))).unwrap();
    for identity in [
        None,
        Some(runtime_identity(false, true)),
        Some(runtime_identity(true, false)),
        Some(runtime_identity(true, true)),
    ] {
        assert_parser_rejects_identity(valid.clone(), identity);
    }
}

#[test]
fn post_reap_failure_requires_the_executed_identity_shape() {
    let valid = failed_execution_payload(Some(runtime_identity(true, true))).unwrap();
    for identity in [
        None,
        Some(runtime_identity(false, false)),
        Some(runtime_identity(false, true)),
        Some(runtime_identity(true, false)),
    ] {
        assert_parser_rejects_identity(valid.clone(), identity);
    }
}

#[test]
fn executable_identity_matches_the_producer_file_contract() {
    let digest = Sha256Digest::from_bytes(b"executable");
    for (file_size_bytes, unix_mode) in [
        (0, Some(0o100_755)),
        (
            RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES + 1,
            Some(0o100_755),
        ),
        (1, Some(0o040_755)),
        (1, Some(0o100_644)),
        (1, None),
    ] {
        assert!(
            failed_execution_payload(Some(RuntimeExecutableIdentity::new(
                file_size_bytes,
                unix_mode,
                digest.clone(),
                true,
                true,
            )))
            .is_err()
        );
    }
    assert!(
        failed_execution_payload(Some(RuntimeExecutableIdentity::new(
            RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES,
            Some(0o100_755),
            digest,
            true,
            true,
        )))
        .is_ok()
    );
}

#[test]
fn selected_output_digest_matches_runtime_version_grammar() {
    for (kind, product) in [
        (LocalExecutableRuntimeKind::CodexExec, "codex-cli 1.2.3"),
        (LocalExecutableRuntimeKind::CodexJsonrpc, "codex-cli 1.2.3"),
        (
            LocalExecutableRuntimeKind::ClaudeCode,
            "1.2.3 (Claude Code)",
        ),
    ] {
        for line_ending in ["", "\n", "\r\n"] {
            let selected = Sha256Digest::from_bytes(format!("{product}{line_ending}").as_bytes());
            let blank = Sha256Digest::from_bytes(b"");
            for stream in [RuntimeVersionStream::Stdout, RuntimeVersionStream::Stderr] {
                let (stdout, stderr) = match stream {
                    RuntimeVersionStream::Stdout => (selected.clone(), blank.clone()),
                    RuntimeVersionStream::Stderr => (blank.clone(), selected.clone()),
                };
                let version =
                    RuntimeVersionFacts::new("1.2.3".to_owned(), stdout, stderr, stream).unwrap();
                assert!(successful_payload(kind, version).is_ok());

                let competing_ending = if line_ending.is_empty() { "\n" } else { "" };
                let competing =
                    Sha256Digest::from_bytes(format!("{product}{competing_ending}").as_bytes());
                let (ambiguous_stdout, ambiguous_stderr) = match stream {
                    RuntimeVersionStream::Stdout => (selected.clone(), competing),
                    RuntimeVersionStream::Stderr => (competing, selected.clone()),
                };
                let ambiguous = RuntimeVersionFacts::new(
                    "1.2.3".to_owned(),
                    ambiguous_stdout,
                    ambiguous_stderr,
                    stream,
                )
                .unwrap();
                assert!(successful_payload(kind, ambiguous).is_err());
            }
        }

        let impossible = RuntimeVersionFacts::new(
            "1.2.3".to_owned(),
            Sha256Digest::from_bytes(b"arbitrary output"),
            Sha256Digest::from_bytes(b""),
            RuntimeVersionStream::Stdout,
        )
        .unwrap();
        assert!(successful_payload(kind, impossible).is_err());
    }
}

#[test]
fn version_product_line_respects_output_capture_ceiling() {
    for (kind, product_prefix, product_suffix) in [
        (LocalExecutableRuntimeKind::CodexExec, "codex-cli ", ""),
        (LocalExecutableRuntimeKind::CodexJsonrpc, "codex-cli ", ""),
        (LocalExecutableRuntimeKind::ClaudeCode, "", " (Claude Code)"),
    ] {
        for line_ending in ["", "\n", "\r\n"] {
            let version_prefix = "1.2.3+";
            let fixed_output_bytes = product_prefix.len()
                + version_prefix.len()
                + product_suffix.len()
                + line_ending.len();
            let normalized_version = format!(
                "{version_prefix}{}",
                "a".repeat(RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES - fixed_output_bytes)
            );
            let product = format!("{product_prefix}{normalized_version}{product_suffix}");
            let exact_output = format!("{product}{line_ending}");
            assert_eq!(exact_output.len(), RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES);

            let oversized_version = format!("{normalized_version}a");
            let oversized_output =
                format!("{product_prefix}{oversized_version}{product_suffix}{line_ending}");
            assert_eq!(
                oversized_output.len(),
                RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES + 1
            );

            for stream in [RuntimeVersionStream::Stdout, RuntimeVersionStream::Stderr] {
                let blank = Sha256Digest::from_bytes(b"");
                let exact_digest = Sha256Digest::from_bytes(exact_output.as_bytes());
                let (stdout, stderr) = match stream {
                    RuntimeVersionStream::Stdout => (exact_digest, blank.clone()),
                    RuntimeVersionStream::Stderr => (blank.clone(), exact_digest),
                };
                let exact =
                    RuntimeVersionFacts::new(normalized_version.clone(), stdout, stderr, stream)
                        .unwrap();
                assert!(successful_payload(kind, exact).is_ok());

                let oversized_digest = Sha256Digest::from_bytes(oversized_output.as_bytes());
                let (stdout, stderr) = match stream {
                    RuntimeVersionStream::Stdout => (oversized_digest, blank.clone()),
                    RuntimeVersionStream::Stderr => (blank.clone(), oversized_digest),
                };
                let oversized =
                    RuntimeVersionFacts::new(oversized_version.clone(), stdout, stderr, stream)
                        .unwrap();
                assert!(successful_payload(kind, oversized).is_err());
            }
        }
    }
}

#[test]
fn failed_candidate_and_pre_checkpoint_failures_forbid_identity() {
    for identity in [runtime_identity(false, false), runtime_identity(true, true)] {
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![runtime_attempt(
                b"failed",
                RuntimeResolutionAttemptOutcome::InspectionFailed,
                RuntimeExecSequence::None,
            )],
            Some(identity.clone()),
            None,
            vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReadFailed).unwrap()],
        )
        .is_err());
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![runtime_attempt(
                b"spawn",
                RuntimeResolutionAttemptOutcome::ExecFailed,
                RuntimeExecSequence::Single,
            )],
            Some(identity),
            None,
            vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::SpawnFailed).unwrap()],
        )
        .is_err());
    }
}

#[test]
fn retained_cleanup_failure_allows_no_post_reap_identity() {
    let attempts = vec![runtime_attempt(
        b"retained",
        RuntimeResolutionAttemptOutcome::ExecStarted,
        RuntimeExecSequence::Single,
    )];
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts.clone(),
        None,
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
        ],
    )
    .is_ok());
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts,
        None,
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
        ],
    )
    .is_err());
}

#[test]
fn lifecycle_cleanup_is_restricted_to_reachable_post_resume_states() {
    let attempts = vec![runtime_attempt(
        b"executed",
        RuntimeResolutionAttemptOutcome::ExecStarted,
        RuntimeExecSequence::Single,
    )];
    let retained = runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts.clone(),
        None,
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap(),
        ],
    )
    .unwrap();
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(retained).unwrap();
    let mut retained_json: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    retained_json["payload"]["executable"] =
        serde_json::to_value(runtime_identity(true, true)).unwrap();
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(
            &serde_json::to_string(&retained_json).unwrap()
        ),
        Err(AgentStackFingerprintError::InvalidPayloadState)
    ));

    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        attempts,
        Some(runtime_identity(true, true)),
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::InvalidUtf8).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputDrainFailed).unwrap(),
        ],
    )
    .is_err());

    let path_failure = runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"absent",
            RuntimeResolutionAttemptOutcome::Absent,
            RuntimeExecSequence::None,
        )],
        None,
        None,
        vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathNotFound).unwrap()],
    )
    .unwrap();
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(path_failure).unwrap();
    let mut path_json: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    path_json["payload"]["failures"]
        .as_array_mut()
        .unwrap()
        .push(
            serde_json::to_value(
                RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap(),
            )
            .unwrap(),
        );
    assert!(matches!(
        AgentStackFingerprintEnvelope::from_json_str(&serde_json::to_string(&path_json).unwrap()),
        Err(AgentStackFingerprintError::InvalidPayloadState)
    ));

    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        vec![runtime_attempt(
            b"mutually-exclusive-cleanup",
            RuntimeResolutionAttemptOutcome::ExecStarted,
            RuntimeExecSequence::Single,
        )],
        None,
        None,
        vec![
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::TerminationFailed).unwrap(),
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::ReapFailed).unwrap(),
        ],
    )
    .is_err());
}

#[test]
fn path_unusable_is_exclusive_to_unreached_unix_bare_resolution() {
    let failure = || vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathUnusable).unwrap()];
    assert!(runtime_payload_with_facts(
        LocalExecutableRuntimeKind::CodexExec,
        RuntimeCommandForm::UnixBare,
        Vec::new(),
        None,
        None,
        failure(),
    )
    .is_ok());
    for form in [
        RuntimeCommandForm::UnixAbsolute,
        RuntimeCommandForm::UnixQualified,
    ] {
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            form,
            Vec::new(),
            None,
            None,
            failure(),
        )
        .is_err());
    }
}

#[test]
fn unix_bare_file_type_skips_cannot_be_terminal_primary_failures() {
    for (outcome, kind) in [
        (
            RuntimeResolutionAttemptOutcome::NotRegular,
            RuntimeProbeFailureKind::NotRegularFile,
        ),
        (
            RuntimeResolutionAttemptOutcome::NotExecutable,
            RuntimeProbeFailureKind::NotExecutable,
        ),
    ] {
        let attempts = vec![runtime_attempt(
            b"skipped",
            outcome,
            RuntimeExecSequence::None,
        )];
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            attempts.clone(),
            None,
            None,
            vec![RuntimeProbeFailure::new(kind).unwrap()],
        )
        .is_err());

        let valid = runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixAbsolute,
            attempts,
            None,
            None,
            vec![RuntimeProbeFailure::new(kind).unwrap()],
        )
        .unwrap();
        let envelope = AgentStackFingerprintEnvelope::agent_runtime(valid).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
        json["payload"]["command_form"] = serde_json::json!("unix_bare");
        assert!(matches!(
            AgentStackFingerprintEnvelope::from_json_str(&serde_json::to_string(&json).unwrap()),
            Err(AgentStackFingerprintError::InvalidPayloadState)
        ));
    }
}

#[test]
fn runtime_environment_digest_rejects_explicit_null() {
    let payload = failed_execution_payload(Some(runtime_identity(true, true))).unwrap();
    let json = AgentStackFingerprintEnvelope::agent_runtime(payload)
        .unwrap()
        .to_json_string()
        .unwrap();
    let invalid = json.replacen(
        "\"state\":\"unset\"",
        "\"state\":\"unset\",\"value_sha256\":null",
        1,
    );
    assert_ne!(invalid, json);
    assert!(AgentStackFingerprintEnvelope::from_json_str(&invalid).is_err());
}

#[test]
fn numeric_failure_details_enforce_kernel_and_public_option_ranges() {
    for code in [i32::MIN, -1, 0, 256, i32::MAX] {
        assert!(RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::NonzeroExit,
            RuntimeProbeFailureDetail::ExitCode(code),
        )
        .is_err());
    }
    for code in [1, 255] {
        assert!(RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::NonzeroExit,
            RuntimeProbeFailureDetail::ExitCode(code),
        )
        .is_ok());
    }
    for limit in [0, 65_537, u64::MAX] {
        assert!(RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::OutputLimitExceeded,
            RuntimeProbeFailureDetail::OutputLimitBytes(limit),
        )
        .is_err());
    }
    for limit in [1, 65_536] {
        assert!(RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::OutputLimitExceeded,
            RuntimeProbeFailureDetail::OutputLimitBytes(limit),
        )
        .is_ok());
    }
}

#[test]
fn parser_rejects_out_of_range_numeric_failure_details() {
    for (kind, detail, invalid_values) in [
        (
            RuntimeProbeFailureKind::NonzeroExit,
            RuntimeProbeFailureDetail::ExitCode(1),
            vec![serde_json::json!(0), serde_json::json!(256)],
        ),
        (
            RuntimeProbeFailureKind::OutputLimitExceeded,
            RuntimeProbeFailureDetail::OutputLimitBytes(1),
            vec![serde_json::json!(0), serde_json::json!(65_537)],
        ),
    ] {
        let payload = runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            RuntimeCommandForm::UnixBare,
            vec![runtime_attempt(
                b"numeric",
                RuntimeResolutionAttemptOutcome::ExecStarted,
                RuntimeExecSequence::Single,
            )],
            Some(runtime_identity(true, true)),
            None,
            vec![RuntimeProbeFailure::with_detail(kind, detail).unwrap()],
        )
        .unwrap();
        let envelope = AgentStackFingerprintEnvelope::agent_runtime(payload).unwrap();
        let valid_json: serde_json::Value =
            serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
        for invalid in invalid_values {
            let mut json = valid_json.clone();
            json["payload"]["failures"][0]["detail"]["value"] = invalid;
            assert!(matches!(
                AgentStackFingerprintEnvelope::from_json_str(
                    &serde_json::to_string(&json).unwrap()
                ),
                Err(AgentStackFingerprintError::InvalidPayloadState)
            ));
        }
    }
}

#[test]
fn v0_1_envelopes_reject_every_windows_command_form() {
    let payload = failed_execution_payload(Some(runtime_identity(true, true))).unwrap();
    let envelope = AgentStackFingerprintEnvelope::agent_runtime(payload).unwrap();
    let valid_json: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    for (form, wire) in [
        (RuntimeCommandForm::WindowsBare, "windows_bare"),
        (RuntimeCommandForm::WindowsAbsolute, "windows_absolute"),
        (RuntimeCommandForm::WindowsQualified, "windows_qualified"),
    ] {
        assert!(runtime_payload_with_facts(
            LocalExecutableRuntimeKind::CodexExec,
            form,
            Vec::new(),
            None,
            None,
            vec![RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathUnusable).unwrap()],
        )
        .is_err());
        let mut json = valid_json.clone();
        json["payload"]["command_form"] = serde_json::json!(wire);
        assert!(matches!(
            AgentStackFingerprintEnvelope::from_json_str(&serde_json::to_string(&json).unwrap()),
            Err(AgentStackFingerprintError::InvalidPayloadState)
        ));
    }
}
