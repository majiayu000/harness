use super::*;
use harness_core::stack::fingerprint::RuntimeCommandForm;
use harness_core::stack::{AgentStackSource, AgentStackSourceScope};
use serde_json::json;

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
        RuntimeFingerprintProduceError::UnsupportedIsolation(IsolationTier::Container)
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
        RuntimeFingerprintProduceError::UnsupportedIsolation(IsolationTier::Microvm)
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
        RuntimeFingerprintProduceError::SandboxParityUnavailable
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
        RuntimeFingerprintProduceError::SandboxParityUnavailable
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
        RuntimeFingerprintProduceError::ContainmentUnavailable(
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

#[test]
fn sandbox_passthrough_state_is_only_supported_policy() {
    let value = configured(IsolationTier::Host, sandbox(SandboxMode::DangerFullAccess));
    assert!(value.validate_execution_boundary().is_ok());
}

#[cfg(unix)]
#[test]
fn unix_command_and_working_directory_digest_vectors_are_fixed() {
    let prepared = executable::prepare_command(
        std::ffi::OsStr::new("codex"),
        Path::new("/x"),
        Some(std::ffi::OsStr::new("/bin")),
    )
    .unwrap();
    assert_eq!(
        prepared.working_directory_digest.as_str(),
        "bdc1de448a5df96390bcc54bf757c96abf628c534baef27bdeba60c5350ebaf6"
    );
    assert_ne!(
        prepared.configured_command_digest,
        prepared.working_directory_digest
    );
}

#[test]
fn windows_working_directory_digest_vector_is_fixed() {
    let units = "C:\\X".encode_utf16().collect::<Vec<_>>();
    assert_eq!(
        windows_working_directory_digest(&units).unwrap().as_str(),
        "90e7e9eb468b08a8b8b5161fb2211bcba076a30439db72f7d6761d6398372085"
    );
}

#[test]
fn working_directory_identity_digest_vector_is_fixed() {
    assert_eq!(
        runtime_working_directory_identity_digest(1, 2).as_str(),
        "0980191ed8a4adfd1d3a83af85fb72a46b9aae6ff342d53517995d161ee7f4f9"
    );
}

#[cfg(unix)]
#[test]
fn unix_command_forms_and_path_order_are_frozen() {
    use executable::CandidateReference;

    let absolute =
        executable::prepare_command(std::ffi::OsStr::new("/opt/codex"), Path::new("/repo"), None)
            .unwrap();
    assert_eq!(absolute.command_form, RuntimeCommandForm::UnixAbsolute);
    assert!(matches!(
        absolute.candidates[0].reference,
        CandidateReference::Absolute(_)
    ));

    let qualified =
        executable::prepare_command(std::ffi::OsStr::new("bin/codex"), Path::new("/repo"), None)
            .unwrap();
    assert_eq!(qualified.command_form, RuntimeCommandForm::UnixQualified);
    assert!(matches!(
        qualified.candidates[0].reference,
        CandidateReference::WorkingDirectoryRelative(_)
    ));

    let bare = executable::prepare_command(
        std::ffi::OsStr::new("codex"),
        Path::new("/repo"),
        Some(std::ffi::OsStr::new(":rel:/abs")),
    )
    .unwrap();
    assert_eq!(bare.command_form, RuntimeCommandForm::UnixBare);
    assert_eq!(bare.candidates.len(), 3);
    assert!(matches!(
        bare.candidates[0].reference,
        CandidateReference::WorkingDirectoryRelative(_)
    ));
    assert!(matches!(
        bare.candidates[2].reference,
        CandidateReference::Absolute(_)
    ));
}

#[cfg(unix)]
#[test]
fn unix_bare_path_candidate_65_is_not_observed() {
    let path = std::iter::repeat_n("entry", 65)
        .collect::<Vec<_>>()
        .join(":");
    let prepared = executable::prepare_command(
        std::ffi::OsStr::new("codex"),
        Path::new("/repo"),
        Some(std::ffi::OsStr::new(&path)),
    )
    .unwrap();
    assert_eq!(prepared.candidates.len(), 64);
    assert!(prepared.candidate_limit_exceeded);
}

#[test]
fn environment_policy_is_closed_and_setup_secret_exclusion_wins() {
    let environment = vec![
        (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
        (OsString::from("PATH"), OsString::from("/bin")),
        (OsString::from("IGNORED"), OsString::from("raw")),
    ];
    let selected = environment::validate_and_select(
        LocalExecutableRuntimeKind::CodexExec,
        &environment,
        &[OsString::from("OPENAI_API_KEY")],
    )
    .unwrap();
    assert_eq!(selected.child_path, Some(OsString::from("/bin")));
    assert_eq!(
        serde_json::to_value(&selected.facts).unwrap(),
        json!([
            {"key":"OPENAI_API_KEY","state":"unset"},
            {"key":"PATH","state":"set_digest","value_sha256":
                "d40b2474349e402a3593aabe8162f447995842e0a88dc3b588ed8124c1b92863"}
        ])
    );
}

#[test]
fn environment_count_precedes_key_validation() {
    let entries = (0..=RUNTIME_FINGERPRINT_MAX_OBSERVATION_ENV_ENTRIES)
        .map(|_| (OsString::new(), OsString::new()))
        .collect::<Vec<_>>();
    let error =
        environment::validate_and_select(LocalExecutableRuntimeKind::CodexExec, &entries, &[])
            .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            RuntimeLaunchInputLimitKind::ObservationEnvironmentEntries
        )
    ));
}

#[cfg(unix)]
#[test]
fn configured_command_and_working_directory_limits_are_exact() {
    use std::os::unix::ffi::OsStringExt;

    let exact = OsString::from_vec(vec![b'a'; RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS]);
    assert_eq!(
        executable::native_os_units_len(&exact, RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS),
        RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS
    );
    let over = OsString::from_vec(vec![b'a'; RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 1]);
    assert_eq!(
        executable::native_os_units_len(&over, RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS),
        RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 1
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_capability_child_is_gated_registered_and_reaped_by_pidfd() {
    let working_directory = std::env::current_dir().unwrap();
    let envelope = fingerprint_configured_runtime_executable(
        &configured(IsolationTier::Host, sandbox(SandboxMode::DangerFullAccess)),
        &RuntimeFingerprintOptions::new(working_directory)
            .with_environment([(OsString::from("PATH"), OsString::from("/bin"))]),
    )
    .await
    .unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(observed["payload"]["failures"][0]["kind"], "path_not_found");
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "absent"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bare_without_sanitized_path_records_path_unusable_after_containment() {
    let envelope = fingerprint_configured_runtime_executable(
        &configured(IsolationTier::Host, sandbox(SandboxMode::DangerFullAccess)),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap()),
    )
    .await
    .unwrap();
    let json = envelope.to_json_string().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["payload"]["failures"][0]["kind"], "path_unusable");
    assert_eq!(parsed["payload"]["resolution_attempts"], json!([]));
    assert!(parsed["payload"]["working_directory_identity_digest"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
}

#[test]
fn runtime_whole_output_grammars_and_stream_selection_are_exact() {
    let codex = classify_completed_runtime_output(
        LocalExecutableRuntimeKind::CodexExec,
        b"codex-cli 1.2.3-alpha-1+Build.7\r\n",
        b" \t\r\n",
        RuntimeTermination::Exit(0),
    )
    .unwrap();
    let claude = classify_completed_runtime_output(
        LocalExecutableRuntimeKind::ClaudeCode,
        b"",
        b"1.2.3 (Claude Code)\n",
        RuntimeTermination::Exit(0),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(codex).unwrap()["normalized_version"],
        "1.2.3-alpha-1+Build.7"
    );
    assert_eq!(
        serde_json::to_value(claude).unwrap()["selected_stream"],
        "stderr"
    );

    for invalid in [
        "codex-cli v1.2.3",
        "codex-cli 01.2.3",
        "codex-cli 1.2.3\r",
        "codex-cli 1.2.3\nextra",
        " codex-cli 1.2.3",
    ] {
        let value = classify_completed_runtime_output(
            LocalExecutableRuntimeKind::CodexExec,
            invalid.as_bytes(),
            b"",
            RuntimeTermination::Exit(0),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap()["kind"],
            "unparseable_version"
        );
    }
}

#[test]
fn runtime_output_exit_precedence_and_blank_predicate_are_closed() {
    let signal = classify_completed_runtime_output(
        LocalExecutableRuntimeKind::CodexExec,
        &[0xff],
        b"codex-cli 1.2.3",
        RuntimeTermination::Signal,
    )
    .unwrap();
    let nonzero = classify_completed_runtime_output(
        LocalExecutableRuntimeKind::CodexExec,
        &[0xff],
        b"",
        RuntimeTermination::Exit(7),
    )
    .unwrap();
    let vertical_tab = classify_completed_runtime_output(
        LocalExecutableRuntimeKind::CodexExec,
        b"codex-cli 1.2.3",
        b"\x0b",
        RuntimeTermination::Exit(0),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(signal).unwrap()["kind"],
        "terminated_by_signal"
    );
    assert_eq!(
        serde_json::to_value(nonzero).unwrap()["detail"]["detail"],
        "exit_code"
    );
    assert_eq!(
        serde_json::to_value(vertical_tab).unwrap()["kind"],
        "unparseable_version"
    );
}

fn static_elf_fixture(machine: u16) -> Vec<u8> {
    let mut image = vec![0_u8; 120];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    image[18..20].copy_from_slice(&machine.to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&1_u16.to_le_bytes());
    image[64..68].copy_from_slice(&0x6474_e551_u32.to_le_bytes());
    image[68..72].copy_from_slice(&6_u32.to_le_bytes());
    image
}

#[test]
fn static_elf_classifier_freezes_architecture_and_program_header_policy() {
    let eligible = static_elf_fixture(62);
    assert_eq!(
        classify_static_linux_elf(&eligible, LinuxElfArchitecture::X86_64),
        LinuxStaticElfClassification::Eligible
    );
    assert_eq!(
        classify_static_linux_elf(&eligible, LinuxElfArchitecture::Aarch64),
        LinuxStaticElfClassification::Unsupported
    );

    let mut interpreter = eligible.clone();
    interpreter[64..68].copy_from_slice(&3_u32.to_le_bytes());
    let mut executable_stack = eligible.clone();
    executable_stack[68..72].copy_from_slice(&7_u32.to_le_bytes());
    let mut writable_executable = eligible.clone();
    writable_executable[64..68].copy_from_slice(&1_u32.to_le_bytes());
    writable_executable[68..72].copy_from_slice(&3_u32.to_le_bytes());
    for unsupported in [interpreter, executable_stack, writable_executable] {
        assert_eq!(
            classify_static_linux_elf(&unsupported, LinuxElfArchitecture::X86_64),
            LinuxStaticElfClassification::Unsupported
        );
    }
}

#[cfg(target_os = "linux")]
fn configured_path(path: &Path) -> ConfiguredRuntimeExecutable {
    ConfiguredRuntimeExecutable::new(
        LocalExecutableRuntimeKind::CodexExec,
        source("candidate"),
        IsolationTier::Host,
        sandbox(SandboxMode::DangerFullAccess),
        path,
        Vec::new(),
    )
}

#[cfg(target_os = "linux")]
async fn fingerprint_failure_for(path: &Path) -> serde_json::Value {
    let envelope = fingerprint_configured_runtime_executable(
        &configured_path(path),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap()),
    )
    .await
    .unwrap();
    serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_candidate_observation_records_closed_skip_and_identity_failures() {
    use std::os::unix::fs::PermissionsExt;

    let missing = fingerprint_failure_for(Path::new("/definitely/missing/harness-runtime")).await;
    assert_eq!(missing["payload"]["failures"][0]["kind"], "path_not_found");
    assert_eq!(
        missing["payload"]["resolution_attempts"][0]["outcome"],
        "absent"
    );

    let directory = tempfile::tempdir().unwrap();
    let non_executable = directory.path().join("non-executable");
    std::fs::write(&non_executable, b"not executable").unwrap();
    std::fs::set_permissions(&non_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let observed = fingerprint_failure_for(&non_executable).await;
    assert_eq!(observed["payload"]["failures"][0]["kind"], "not_executable");
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "not_executable"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_candidate_observation_rejects_interpreters_before_authorization() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("script");
    std::fs::write(&script, b"#!/bin/sh\necho forbidden\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let observed = fingerprint_failure_for(&script).await;
    assert_eq!(
        observed["payload"]["failures"][0]["kind"],
        "interpreter_authorization_unavailable"
    );
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "interpreter_authorization_unavailable"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_retained_static_candidate_requires_repository_boundaries() {
    use std::os::unix::fs::PermissionsExt;

    let machine = if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    };
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("static-elf");
    std::fs::write(&executable, static_elf_fixture(machine)).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let observed = fingerprint_failure_for(&executable).await;
    assert_eq!(
        observed["payload"]["failures"][0]["kind"],
        "target_authorization_unavailable"
    );
    assert_eq!(
        observed["payload"]["failures"][0]["detail"]["detail"],
        "boundary_unprovable"
    );
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "authorization_unavailable"
    );
}

#[cfg(target_os = "linux")]
fn write_static_fixture(directory: &Path, name: &str) -> PathBuf {
    super::test_fixtures::write_version_fixture(directory, name)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_target_authorization_classifies_repository_and_external_handles() {
    let repository = tempfile::tempdir().unwrap();
    let repository_executable = write_static_fixture(repository.path(), "repository-runtime");
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        [&repository.path()],
    )
    .unwrap();
    let envelope = fingerprint_configured_runtime_executable(
        &configured_path(&repository_executable),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
            .with_repository_boundaries(boundaries),
    )
    .await
    .unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(
        observed["payload"]["failures"][0]["kind"],
        "probe_not_authorized"
    );
    assert_eq!(
        observed["payload"]["failures"][0]["detail"]["detail"],
        "resolved_target_repository"
    );
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "inspection_target"
    );

    let external = tempfile::tempdir().unwrap();
    let external_executable = write_static_fixture(external.path(), "external-runtime");
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    let envelope = fingerprint_configured_runtime_executable(
        &configured_path(&external_executable),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
            .with_repository_boundaries(boundaries),
    )
    .await
    .unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "exec_started"
    );
    assert_eq!(observed["payload"]["failures"], serde_json::json!([]));
    assert_eq!(
        observed["payload"]["version"]["normalized_version"],
        "1.2.3"
    );
    assert_eq!(
        observed["payload"]["executable"]["checkpoint_consistent_path"],
        true
    );
    assert_eq!(
        observed["payload"]["executable"]["exec_stop_consistent_handle"],
        true
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_target_authorization_rejects_multiple_hard_links() {
    let repository = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let executable = write_static_fixture(external.path(), "linked-runtime");
    std::fs::hard_link(&executable, external.path().join("second-link")).unwrap();
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    let envelope = fingerprint_configured_runtime_executable(
        &configured_path(&executable),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
            .with_repository_boundaries(boundaries),
    )
    .await
    .unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(
        observed["payload"]["failures"][0]["kind"],
        "target_authorization_unavailable"
    );
    assert_eq!(
        observed["payload"]["failures"][0]["detail"]["detail"],
        "multiple_hard_links"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_pre_spawn_checkpoint_detects_in_place_content_change() {
    let directory = tempfile::tempdir().unwrap();
    let repository = tempfile::tempdir().unwrap();
    let executable = write_static_fixture(directory.path(), "changing-runtime");
    let deadline = std::time::Instant::now() + RUNTIME_FINGERPRINT_PROBE_DEADLINE;
    let working_directory =
        executable::observe_working_directory(directory.path(), deadline).unwrap();
    let command =
        executable::prepare_command(executable.as_os_str(), directory.path(), None).unwrap();
    let retained =
        match candidate::observe_candidate(&command.candidates[0], &working_directory, deadline)
            .unwrap()
        {
            candidate::CandidateObservation::Retained(retained) => retained,
            other => panic!("expected retained candidate, got {other:?}"),
        };
    let mut changed = static_elf_fixture(if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    });
    changed.push(1);
    std::fs::write(&executable, changed).unwrap();
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    assert_eq!(
        checkpoint::pre_spawn(
            &command.candidates[0],
            &working_directory,
            &retained,
            &boundaries,
            deadline,
        )
        .unwrap(),
        checkpoint::PreSpawnCheckpoint::IdentityChanged
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_syscall_guard_freezes_every_denied_class_and_safe_queries() {
    use harness_core::stack::fingerprint::RuntimeProbeFailureDetail as D;

    let mut arguments = [0_u64; 6];
    #[cfg(target_arch = "x86_64")]
    let cases = [
        (56, D::ProcessCreation),
        (59, D::ImageExecution),
        (134, D::ExecutableMapping),
        (437, D::ExecutableImageMutation),
        (321, D::KernelCodeLoading),
        (62, D::ProcessSignalling),
    ];
    #[cfg(target_arch = "aarch64")]
    let cases = [
        (220, D::ProcessCreation),
        (221, D::ImageExecution),
        (437, D::ExecutableImageMutation),
        (280, D::KernelCodeLoading),
        (129, D::ProcessSignalling),
    ];
    for (number, expected) in cases {
        assert_eq!(
            syscall_guard::denied_class(number, arguments),
            Ok(Some(expected))
        );
    }
    #[cfg(target_arch = "aarch64")]
    {
        arguments[2] = libc::PROT_EXEC as u64;
        assert_eq!(
            syscall_guard::denied_class(222, arguments),
            Ok(Some(D::ExecutableMapping))
        );
    }
    arguments = [0; 6];
    arguments[0] = u64::from(u32::MAX);
    let personality = if cfg!(target_arch = "x86_64") {
        135
    } else {
        92
    };
    assert_eq!(
        syscall_guard::denied_class(personality, arguments),
        Ok(None)
    );
    assert_eq!(syscall_guard::denied_class(0, [0; 6]), Ok(None));

    #[cfg(target_arch = "x86_64")]
    assert_eq!(syscall_guard::denied_class(0x4000_0000, [0; 6]), Err(()));
}
