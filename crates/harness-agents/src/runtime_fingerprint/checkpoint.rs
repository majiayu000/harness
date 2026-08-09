//! Pre-spawn retained-handle and candidate-path checkpoint.

use super::candidate::RetainedExecutable;
use super::executable::{CandidateReference, ResolvedCandidate, RetainedWorkingDirectory};
use super::{
    RuntimeFingerprintProduceError, RuntimeObservationProtocolReason, RuntimeObservationStage,
    ValidatedRepositoryBoundarySet,
};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

const CONSISTENT: u8 = 1;
const IDENTITY_CHANGED: u8 = 2;
const RESOLVED_TARGET_REPOSITORY: u8 = 3;
const BOUNDARY_UNPROVABLE: u8 = 4;
const LINK_COUNT_UNPROVABLE: u8 = 5;
const UNLINKED_TARGET: u8 = 6;
const MULTIPLE_HARD_LINKS: u8 = 7;
const FINAL_PATH_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreSpawnCheckpoint {
    Consistent,
    IdentityChanged,
    ResolvedTargetRepository,
    BoundaryUnprovable,
    LinkCountUnprovable,
    UnlinkedTarget,
    MultipleHardLinks,
}

struct CheckpointContext<'a> {
    executable: &'a RetainedExecutable,
    working_directory_fd: Option<libc::c_int>,
    candidate_path: &'a CString,
    proc_fd_path: &'a CString,
    boundaries: &'a [CString],
    expected_digest: [u8; 32],
}

pub(super) fn pre_spawn(
    candidate: &ResolvedCandidate,
    working_directory: &RetainedWorkingDirectory,
    executable: &RetainedExecutable,
    boundaries: &ValidatedRepositoryBoundarySet,
    deadline: Instant,
) -> Result<PreSpawnCheckpoint, RuntimeFingerprintProduceError> {
    checkpoint(
        candidate,
        working_directory,
        executable,
        boundaries,
        deadline,
        RuntimeObservationStage::PreSpawnCheckpoint,
    )
}

pub(super) fn post_reap(
    candidate: &ResolvedCandidate,
    working_directory: &RetainedWorkingDirectory,
    executable: &RetainedExecutable,
    boundaries: &ValidatedRepositoryBoundarySet,
    deadline: Instant,
) -> Result<PreSpawnCheckpoint, RuntimeFingerprintProduceError> {
    checkpoint(
        candidate,
        working_directory,
        executable,
        boundaries,
        deadline,
        RuntimeObservationStage::PostReapCheckpoint,
    )
}

fn checkpoint(
    candidate: &ResolvedCandidate,
    working_directory: &RetainedWorkingDirectory,
    executable: &RetainedExecutable,
    boundaries: &ValidatedRepositoryBoundarySet,
    deadline: Instant,
    stage: RuntimeObservationStage,
) -> Result<PreSpawnCheckpoint, RuntimeFingerprintProduceError> {
    let (working_directory_fd, path) = match &candidate.reference {
        CandidateReference::Absolute(path) => (None, path),
        CandidateReference::WorkingDirectoryRelative(path) => (Some(working_directory.fd()), path),
    };
    let candidate_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let proc_fd_path = CString::new(format!("/proc/self/fd/{}", executable.fd()))
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let boundary_strings = boundaries
        .roots()
        .iter()
        .map(|root| CString::new(root.as_os_str().as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let expected_digest = decode_digest(executable.executable_sha256.as_str())
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    run_child(
        &CheckpointContext {
            executable,
            working_directory_fd,
            candidate_path: &candidate_path,
            proc_fd_path: &proc_fd_path,
            boundaries: &boundary_strings,
            expected_digest,
        },
        deadline,
        stage,
    )
}

fn run_child(
    context: &CheckpointContext<'_>,
    deadline: Instant,
    stage: RuntimeObservationStage,
) -> Result<PreSpawnCheckpoint, RuntimeFingerprintProduceError> {
    let role = super::RuntimeOwnedChildRole::Observation(stage);
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
    let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut saved = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mask_result = unsafe {
        libc::sigfillset(&mut blocked);
        libc::sigdelset(&mut blocked, libc::SIGKILL);
        libc::sigdelset(&mut blocked, libc::SIGSTOP);
        libc::pthread_sigmask(libc::SIG_SETMASK, &blocked, &mut saved)
    };
    if mask_result != 0 {
        close_all(gate, status, protocol);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_checkpoint(gate, status, protocol, context);
    }
    let restored =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) };
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    super::probe::close_fd(protocol[1]);
    if pid < 0 || restored != 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        super::probe::close_fd(protocol[0]);
        if pid > 0 {
            super::probe::rollback_unregistered_child(pid, deadline, role)?;
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
        super::probe::rollback_unregistered_child(pid, deadline, role)?;
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
        super::probe::rollback_unregistered_child(pid, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    if super::probe::write_byte(gate[1], super::probe::CHILD_GO).is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        super::probe::cleanup_registered_child(pidfd, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateRelease,
        ));
    }
    super::probe::close_fd(gate[1]);
    let checkpoint = receive(protocol[0], deadline, stage);
    super::probe::close_fd(protocol[0]);
    let exited = super::probe::waitid_pidfd(pidfd, false, deadline);
    if !matches!(exited, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup_deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
        super::probe::cleanup_registered_child(pidfd, cleanup_deadline, role)?;
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage,
            reason: RuntimeObservationProtocolReason::HelperExited,
        });
    }
    super::probe::close_fd(pidfd);
    checkpoint
}

fn child_checkpoint(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
    context: &CheckpointContext<'_>,
) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let isolation = super::candidate::child_isolate(
        gate[0],
        status[1],
        protocol[1],
        [Some(context.executable.fd()), context.working_directory_fd],
    );
    if isolation != super::probe::CHILD_READY {
        child_status_exit(status[1], isolation);
    }
    if !super::candidate::child_install_empty_mask() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(status[1], super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(151) };
    }
    let mut go = 0_u8;
    if unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) } != 1
        || go != super::probe::CHILD_GO
    {
        unsafe { libc::_exit(152) };
    }
    let mut retained_metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(context.executable.fd(), &mut retained_metadata) } != 0 {
        child_send(protocol[1], LINK_COUNT_UNPROVABLE);
    }
    let retained_links = super::candidate::stat_link_count(&retained_metadata);
    if retained_links == 0 {
        child_send(protocol[1], UNLINKED_TARGET);
    }
    if retained_links > 1 {
        child_send(protocol[1], MULTIPLE_HARD_LINKS);
    }
    if !metadata_matches(&retained_metadata, context.executable)
        || super::candidate::child_checkpoint_hash(context.executable.fd())
            != Some(context.expected_digest)
    {
        child_send(protocol[1], IDENTITY_CHANGED);
    }
    let mut final_path = [0_u8; FINAL_PATH_BYTES];
    let path_bytes = unsafe {
        libc::readlink(
            context.proc_fd_path.as_ptr(),
            final_path.as_mut_ptr().cast(),
            final_path.len(),
        )
    };
    if path_bytes <= 0 || path_bytes as usize == final_path.len() {
        child_send(protocol[1], BOUNDARY_UNPROVABLE);
    }
    let final_path = &final_path[..path_bytes as usize];
    if final_path.ends_with(b" (deleted)") {
        child_send(protocol[1], UNLINKED_TARGET);
    }
    for boundary in context.boundaries {
        let root = boundary.as_bytes();
        if final_path == root
            || (final_path.starts_with(root)
                && (root == b"/" || final_path.get(root.len()) == Some(&b'/')))
        {
            child_send(protocol[1], RESOLVED_TARGET_REPOSITORY);
        }
    }
    let directory_fd = context.working_directory_fd.unwrap_or(libc::AT_FDCWD);
    let reopened = unsafe {
        libc::openat(
            directory_fd,
            context.candidate_path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        )
    };
    if reopened < 0 {
        child_send(protocol[1], IDENTITY_CHANGED);
    }
    let mut path_metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(reopened, &mut path_metadata) } != 0
        || path_metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || path_metadata.st_mode & 0o111 == 0
        || !metadata_matches(&path_metadata, context.executable)
    {
        child_send(protocol[1], IDENTITY_CHANGED);
    }
    child_send(protocol[1], CONSISTENT);
}

fn metadata_matches(metadata: &libc::stat, executable: &RetainedExecutable) -> bool {
    metadata.st_dev == executable.device
        && metadata.st_ino == executable.inode
        && super::candidate::stat_link_count(metadata) == executable.link_count
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
        digest[index] = hex_nibble(pair[0])?
            .checked_mul(16)?
            .checked_add(hex_nibble(pair[1])?)?;
    }
    Some(digest)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(150) }
}

fn child_send(fd: libc::c_int, value: u8) -> ! {
    let result = super::probe::write_byte(fd, value);
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 153 }) }
}

fn receive(
    fd: libc::c_int,
    deadline: Instant,
    stage: RuntimeObservationStage,
) -> Result<PreSpawnCheckpoint, RuntimeFingerprintProduceError> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pollfd, 1, remaining.min(i32::MAX as u128) as _) } <= 0 {
        return Err(RuntimeFingerprintProduceError::ObservationDeadlineExceeded { stage });
    }
    let mut frame = [0_u8; 2];
    let received = unsafe { libc::recv(fd, frame.as_mut_ptr().cast(), frame.len(), 0) };
    if received != 1 {
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage,
            reason: if received > 1 {
                RuntimeObservationProtocolReason::SurplusFields
            } else {
                RuntimeObservationProtocolReason::TruncatedFrame
            },
        });
    }
    match frame[0] {
        CONSISTENT => Ok(PreSpawnCheckpoint::Consistent),
        IDENTITY_CHANGED => Ok(PreSpawnCheckpoint::IdentityChanged),
        RESOLVED_TARGET_REPOSITORY => Ok(PreSpawnCheckpoint::ResolvedTargetRepository),
        BOUNDARY_UNPROVABLE => Ok(PreSpawnCheckpoint::BoundaryUnprovable),
        LINK_COUNT_UNPROVABLE => Ok(PreSpawnCheckpoint::LinkCountUnprovable),
        UNLINKED_TARGET => Ok(PreSpawnCheckpoint::UnlinkedTarget),
        MULTIPLE_HARD_LINKS => Ok(PreSpawnCheckpoint::MultipleHardLinks),
        _ => Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage,
            reason: RuntimeObservationProtocolReason::SurplusFields,
        }),
    }
}

fn close_all(gate: [libc::c_int; 2], status: [libc::c_int; 2], protocol: [libc::c_int; 2]) {
    super::probe::close_pipe_pair(gate);
    super::probe::close_pipe_pair(status);
    super::probe::close_pipe_pair(protocol);
}
