//! Strong-identity observation while the target remains stopped at exec.

use super::candidate::RetainedExecutable;
use super::{RuntimeFingerprintProduceError, RuntimeObservationProtocolReason};
use std::ffi::CString;
use std::time::Instant;

const CONSISTENT: u8 = 1;
const IDENTITY_CHANGED: u8 = 2;
const VERIFICATION_UNAVAILABLE: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecStopCheckpoint {
    Consistent,
    IdentityChanged,
}

struct ExecStopContext<'a> {
    executable: &'a RetainedExecutable,
    image_path: &'a CString,
    expected_digest: [u8; 32],
}

pub(super) fn verify(
    target_pid: libc::pid_t,
    executable: &RetainedExecutable,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<ExecStopCheckpoint, RuntimeFingerprintProduceError> {
    let image_path = CString::new(format!("/proc/{target_pid}/exe"))
        .map_err(|_| RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)?;
    let expected_digest = decode_digest(executable.executable_sha256.as_str())
        .ok_or(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)?;
    run_child(
        &ExecStopContext {
            executable,
            image_path: &image_path,
            expected_digest,
        },
        deadline,
        registry,
    )
}

fn run_child(
    context: &ExecStopContext<'_>,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<ExecStopCheckpoint, RuntimeFingerprintProduceError> {
    let _descriptor_lease = registry.reserve_descriptors(6)?;
    let role = super::RuntimeOwnedChildRole::Observation(
        super::RuntimeObservationStage::ExecStopCheckpoint,
    );
    let mut gate = [-1; 2];
    let mut status = [-1; 2];
    let mut protocol = [-1; 2];
    let created = unsafe {
        libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) == 0
            && libc::pipe2(status.as_mut_ptr(), libc::O_CLOEXEC) == 0
            && libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                protocol.as_mut_ptr(),
            ) == 0
    };
    if !created {
        close_all(gate, status, protocol);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateCreate,
        ));
    }
    let saved_signal_mask = super::probe::block_all_signals().map_err(|()| {
        close_all(gate, status, protocol);
        super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        )
    })?;
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_verify(gate, status, protocol, context);
    }
    let restored = super::probe::restore_signal_mask(saved_signal_mask);
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    super::probe::close_fd(protocol[1]);
    if pid < 0 || restored.is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        super::probe::close_fd(protocol[0]);
        if pid > 0 {
            super::probe::rollback_unregistered_child(registry, pid, deadline, role)?;
        }
        return Err(super::probe::registration_error(
            role,
            if pid < 0 {
                super::RuntimeChildRegistrationStage::Fork
            } else {
                super::RuntimeChildRegistrationStage::SignalIsolation
            },
        ));
    }
    let ready = super::probe::read_byte_before(status[0], deadline);
    super::probe::close_fd(status[0]);
    if ready != Ok(super::probe::CHILD_READY) {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        super::probe::rollback_unregistered_child(registry, pid, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            match ready {
                Ok(super::probe::CHILD_SIGNAL_FAILED) => {
                    super::RuntimeChildRegistrationStage::SignalIsolation
                }
                _ => super::RuntimeChildRegistrationStage::DescriptorIsolation,
            },
        ));
    }
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    if pidfd < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        super::probe::rollback_unregistered_child(registry, pid, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    let child = match super::probe::register_child(registry, pid, pidfd, deadline, role) {
        Ok(child) => child,
        Err(error) => {
            super::probe::close_fd(gate[1]);
            super::probe::close_fd(protocol[0]);
            return Err(error);
        }
    };
    if super::probe::write_byte(gate[1], super::probe::CHILD_GO).is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        child.cleanup(deadline)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateRelease,
        ));
    }
    super::probe::close_fd(gate[1]);
    let observed = receive(protocol[0], deadline);
    super::probe::close_fd(protocol[0]);
    let exited = super::probe::waitid_pidfd(child.pidfd(), false, deadline);
    if !matches!(exited, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup_deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
        child.cleanup(cleanup_deadline)?;
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::ExecStopCheckpoint,
            reason: RuntimeObservationProtocolReason::HelperExited,
        });
    }
    child.reaped()?;
    observed
}

fn child_verify(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
    context: &ExecStopContext<'_>,
) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let isolation = super::candidate::child_isolate(
        gate[0],
        status[1],
        protocol[1],
        [Some(context.executable.fd()), None],
    );
    if isolation != super::probe::CHILD_READY {
        child_status_exit(status[1], isolation);
    }
    if !super::candidate::child_install_empty_mask() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(status[1], super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(171) };
    }
    let mut go = 0_u8;
    if unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) } != 1
        || go != super::probe::CHILD_GO
    {
        unsafe { libc::_exit(172) };
    }
    let mut retained = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(context.executable.fd(), &mut retained) } != 0 {
        child_send(protocol[1], VERIFICATION_UNAVAILABLE);
    }
    if !metadata_matches(&retained, context.executable)
        || super::candidate::stat_link_count(&retained) != 1
        || super::candidate::child_checkpoint_hash(context.executable.fd())
            != Some(context.expected_digest)
    {
        child_send(protocol[1], IDENTITY_CHANGED);
    }
    let image = unsafe {
        libc::open(
            context.image_path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        )
    };
    if image < 0 {
        child_send(protocol[1], VERIFICATION_UNAVAILABLE);
    }
    let mut image_metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(image, &mut image_metadata) } != 0 {
        child_send(protocol[1], VERIFICATION_UNAVAILABLE);
    }
    if !metadata_matches(&image_metadata, context.executable)
        || super::candidate::stat_link_count(&image_metadata) != 1
    {
        child_send(protocol[1], IDENTITY_CHANGED);
    }
    child_send(protocol[1], CONSISTENT);
}

fn metadata_matches(metadata: &libc::stat, executable: &RetainedExecutable) -> bool {
    metadata.st_dev == executable.device
        && metadata.st_ino == executable.inode
        && metadata.st_size >= 0
        && metadata.st_size as u64 == executable.file_size_bytes
        && metadata.st_mode == executable.unix_mode
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = hex(pair[0])?.checked_mul(16)?.checked_add(hex(pair[1])?)?;
    }
    Some(digest)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(170) }
}

fn child_send(fd: libc::c_int, status: u8) -> ! {
    let result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 173 }) }
}

fn receive(
    fd: libc::c_int,
    deadline: Instant,
) -> Result<ExecStopCheckpoint, RuntimeFingerprintProduceError> {
    let value = super::probe::read_byte_before(fd, deadline).map_err(|_| {
        RuntimeFingerprintProduceError::ObservationDeadlineExceeded {
            stage: super::RuntimeObservationStage::ExecStopCheckpoint,
        }
    })?;
    match value {
        CONSISTENT => Ok(ExecStopCheckpoint::Consistent),
        IDENTITY_CHANGED => Ok(ExecStopCheckpoint::IdentityChanged),
        VERIFICATION_UNAVAILABLE => {
            Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
        }
        _ => Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::ExecStopCheckpoint,
            reason: RuntimeObservationProtocolReason::SurplusFields,
        }),
    }
}

fn close_all(gate: [libc::c_int; 2], status: [libc::c_int; 2], protocol: [libc::c_int; 2]) {
    super::probe::close_pipe_pair(gate);
    super::probe::close_pipe_pair(status);
    super::probe::close_pipe_pair(protocol);
}
