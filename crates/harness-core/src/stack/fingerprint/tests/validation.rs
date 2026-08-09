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
