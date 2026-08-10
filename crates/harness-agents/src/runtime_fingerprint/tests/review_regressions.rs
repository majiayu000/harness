use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn fixed_frame_failures_keep_their_closed_protocol_reasons() {
    use RuntimeObservationProtocolReason as Reason;

    assert_eq!(probe::fixed_frame_protocol_reason(4, 4, 0), None);
    assert_eq!(
        probe::fixed_frame_protocol_reason(3, 4, 0),
        Some(Reason::TruncatedFrame)
    );
    assert_eq!(
        probe::fixed_frame_protocol_reason(4, 4, libc::MSG_TRUNC),
        Some(Reason::OversizedFrame)
    );
    assert_eq!(
        probe::fixed_frame_protocol_reason(4, 4, libc::MSG_CTRUNC),
        Some(Reason::DescriptorCountMismatch)
    );
}

#[test]
fn seqpacket_receiver_reports_an_oversized_packet() {
    let mut sockets = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                sockets.as_mut_ptr(),
            )
        },
        0
    );
    let payload = [1_u8, 2];
    assert_eq!(
        unsafe { libc::send(sockets[0], payload.as_ptr().cast(), payload.len(), 0) },
        2
    );
    let mut frame = [0_u8; 1];
    let error = probe::receive_exact_frame(
        sockets[1],
        &mut frame,
        RuntimeObservationStage::TargetAuthorization,
    )
    .unwrap_err();
    probe::close_pipe_pair(sockets);
    assert!(matches!(
        error,
        RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            reason: RuntimeObservationProtocolReason::OversizedFrame,
            ..
        }
    ));
}

#[test]
fn recvmsg_retries_eintr_and_classifies_other_syscall_failures() {
    let mut sockets = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                sockets.as_mut_ptr(),
            )
        },
        0
    );
    assert_eq!(
        unsafe { libc::send(sockets[0], c"x".as_ptr().cast(), 1, 0) },
        1
    );
    let mut frame = [0_u8; 1];
    let mut iov = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let mut calls = 0;
    let received = probe::recvmsg_retry_with(
        &mut message,
        RuntimeObservationStage::TargetAuthorization,
        |message| {
            calls += 1;
            if calls == 1 {
                unsafe { *libc::__errno_location() = libc::EINTR };
                -1
            } else {
                unsafe { libc::recvmsg(sockets[1], message, 0) }
            }
        },
    )
    .unwrap();
    assert_eq!(received, 1);
    assert_eq!(calls, 2);
    assert_eq!(frame, [b'x']);
    probe::close_pipe_pair(sockets);

    let mut frame = [0_u8; 1];
    let error =
        probe::receive_exact_frame(-1, &mut frame, RuntimeObservationStage::TargetAuthorization)
            .unwrap_err();
    assert!(matches!(
        error,
        RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            reason: RuntimeObservationProtocolReason::HelperExited,
            ..
        }
    ));
}

#[test]
fn parent_signal_mask_failures_are_containment_failures() {
    assert!(matches!(
        probe::parent_signal_isolation_error(),
        RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::SignalIsolationUnavailable
        )
    ));
}

#[test]
fn readiness_failures_preserve_deadline_and_channel_semantics() {
    let observation_role =
        RuntimeOwnedChildRole::Observation(RuntimeObservationStage::TargetAuthorization);
    assert!(matches!(
        probe::readiness_error(observation_role, probe::StatusReadError::Deadline),
        RuntimeFingerprintProduceError::ObservationDeadlineExceeded {
            stage: RuntimeObservationStage::TargetAuthorization
        }
    ));
    assert!(matches!(
        probe::readiness_error(
            RuntimeOwnedChildRole::InitialTarget,
            probe::StatusReadError::Deadline,
        ),
        RuntimeFingerprintProduceError::ExecutionVerificationUnavailable
    ));
    assert!(matches!(
        probe::readiness_error(observation_role, probe::StatusReadError::Channel),
        RuntimeFingerprintProduceError::ChildRegistrationUnavailable {
            role: RuntimeOwnedChildRole::Observation(RuntimeObservationStage::TargetAuthorization),
            stage: RuntimeChildRegistrationStage::DescriptorIsolation,
        }
    ));
}

#[test]
fn repository_boundaries_require_directories_for_every_root() {
    let repository = tempfile::tempdir().unwrap();
    let file = repository.path().join("not-a-directory");
    std::fs::write(&file, b"boundary").unwrap();
    assert!(ValidatedRepositoryBoundarySet::from_existing_roots(
        &file,
        std::iter::empty::<&std::path::Path>(),
    )
    .is_err());
    assert!(
        ValidatedRepositoryBoundarySet::from_existing_roots(repository.path(), [&file],).is_err()
    );
    assert!(ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        [repository.path()],
    )
    .is_ok());
}

#[test]
fn poll_wait_wakes_for_delayed_pipe_data_and_preserves_timeout() {
    let mut pipe = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
        0
    );
    let writer = pipe[1];
    let delayed_write = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(unsafe { libc::write(writer, c"x".as_ptr().cast(), 1) }, 1);
        probe::close_fd(writer);
    });
    let mut descriptor = [libc::pollfd {
        fd: pipe[0],
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    }];
    assert_eq!(
        probe::poll_until_ready(
            &mut descriptor,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        ),
        Ok(())
    );
    assert!(delayed_write.join().is_ok());
    probe::close_fd(pipe[0]);

    let mut timeout = [libc::pollfd {
        fd: -1,
        events: libc::POLLIN,
        revents: 0,
    }];
    assert_eq!(
        probe::poll_until_ready(&mut timeout, std::time::Instant::now()),
        Err(probe::PollFailure::Timeout)
    );
}

#[tokio::test]
async fn repository_inspection_retains_the_exact_nonexecuted_identity() {
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
    let expected_metadata = std::fs::metadata(&repository_executable).unwrap();
    assert_eq!(
        observed["payload"]["executable"]["file_size_bytes"],
        expected_metadata.len()
    );
    assert_eq!(
        observed["payload"]["executable"]["unix_mode"],
        expected_metadata.permissions().mode()
    );
    assert_eq!(
        observed["payload"]["executable"]["executable_sha256"],
        harness_core::stack::Sha256Digest::from_bytes(
            &std::fs::read(&repository_executable).unwrap()
        )
        .as_str()
    );
    assert_eq!(
        observed["payload"]["executable"]["checkpoint_consistent_path"],
        false
    );
    assert_eq!(
        observed["payload"]["executable"]["exec_stop_consistent_handle"],
        false
    );
}

#[tokio::test]
async fn external_execution_retains_the_post_reap_identity() {
    let repository = tempfile::tempdir().unwrap();
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

#[tokio::test]
async fn post_resume_output_limit_failure_retains_the_post_reap_identity() {
    let repository = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let external_executable = write_static_fixture(external.path(), "limited-runtime");
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    let envelope = fingerprint_configured_runtime_executable(
        &configured_path(&external_executable),
        &RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
            .with_repository_boundaries(boundaries)
            .with_max_output_bytes(1),
    )
    .await
    .unwrap();
    let observed: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(
        observed["payload"]["resolution_attempts"][0]["outcome"],
        "exec_started"
    );
    assert_eq!(
        observed["payload"]["failures"][0]["kind"],
        "output_limit_exceeded"
    );
    assert!(observed["payload"].get("version").is_none());
    assert_eq!(
        observed["payload"]["executable"]["checkpoint_consistent_path"],
        true
    );
    assert_eq!(
        observed["payload"]["executable"]["exec_stop_consistent_handle"],
        true
    );
}
