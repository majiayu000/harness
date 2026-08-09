//! Final-path repository authorization for a retained Linux executable.

use super::candidate::RetainedExecutable;
use super::{
    RuntimeFingerprintProduceError, RuntimeObservationProtocolReason,
    ValidatedRepositoryBoundarySet,
};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

const AUTHORIZED: u8 = 1;
const RESOLVED_TARGET_REPOSITORY: u8 = 2;
const BOUNDARY_UNPROVABLE: u8 = 3;
const LINK_COUNT_UNPROVABLE: u8 = 4;
const UNLINKED_TARGET: u8 = 5;
const MULTIPLE_HARD_LINKS: u8 = 6;
const IDENTITY_CHANGED: u8 = 7;
const FINAL_PATH_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetAuthorization {
    Authorized,
    ResolvedTargetRepository,
    BoundaryUnprovable,
    LinkCountUnprovable,
    UnlinkedTarget,
    MultipleHardLinks,
}

struct AuthorizationContext<'a> {
    executable: &'a RetainedExecutable,
    proc_fd_path: &'a CString,
    boundaries: &'a [CString],
}

pub(super) fn authorize_target(
    executable: &RetainedExecutable,
    boundaries: &ValidatedRepositoryBoundarySet,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<TargetAuthorization, RuntimeFingerprintProduceError> {
    let _descriptor_lease = registry.reserve_descriptors(6)?;
    let proc_fd_path = CString::new(format!("/proc/self/fd/{}", executable.fd()))
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let boundary_strings = boundaries
        .roots()
        .iter()
        .map(|root| CString::new(root.as_os_str().as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let context = AuthorizationContext {
        executable,
        proc_fd_path: &proc_fd_path,
        boundaries: &boundary_strings,
    };
    run_authorization_child(&context, deadline, registry)
}

fn run_authorization_child(
    context: &AuthorizationContext<'_>,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<TargetAuthorization, RuntimeFingerprintProduceError> {
    let role = super::RuntimeOwnedChildRole::Observation(
        super::RuntimeObservationStage::TargetAuthorization,
    );
    let pidfd_lease = registry.reserve_child_pidfd(role)?;
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
    let saved_signal_mask = match super::probe::block_all_signals() {
        Ok(saved) => saved,
        Err(()) => {
            close_all(gate, status, protocol);
            return Err(super::probe::parent_signal_isolation_error());
        }
    };
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_authorize(gate, status, protocol, context);
    }
    let restored = super::probe::restore_signal_mask(saved_signal_mask);
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    super::probe::close_fd(protocol[1]);
    if restored.is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        super::probe::close_fd(protocol[0]);
        if pid > 0 {
            super::probe::rollback_unregistered_child(registry, pid, role)?;
        }
        return Err(super::probe::parent_signal_isolation_error());
    }
    if pid < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        super::probe::close_fd(protocol[0]);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::Fork,
        ));
    }
    let ready = super::probe::read_byte_before(status[0], deadline);
    super::probe::close_fd(status[0]);
    if ready != Ok(super::probe::CHILD_READY) {
        let error = match ready {
            Ok(super::probe::CHILD_SIGNAL_FAILED) => super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::SignalIsolation,
            ),
            Ok(_) => super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::DescriptorIsolation,
            ),
            Err(error) => super::probe::readiness_error(role, error),
        };
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        super::probe::rollback_unregistered_child(registry, pid, role)?;
        return Err(error);
    }
    let pidfd = super::probe::open_child_pidfd(pid);
    if pidfd < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(protocol[0]);
        super::probe::rollback_unregistered_child(registry, pid, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    let child = match super::probe::register_child(registry, pidfd_lease, pid, pidfd, role) {
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
        super::probe::cleanup_registered_child(child)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateRelease,
        ));
    }
    super::probe::close_fd(gate[1]);
    let authorization = receive_authorization(protocol[0], deadline);
    super::probe::close_fd(protocol[0]);
    let exited = super::probe::waitid_pidfd(child.pidfd(), false, deadline);
    if !matches!(exited, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup_deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
        child.cleanup(cleanup_deadline)?;
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::TargetAuthorization,
            reason: RuntimeObservationProtocolReason::HelperExited,
        });
    }
    child.reaped()?;
    authorization
}

fn child_authorize(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
    context: &AuthorizationContext<'_>,
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
        unsafe { libc::_exit(141) };
    }
    let mut go = 0_u8;
    if unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) } != 1
        || go != super::probe::CHILD_GO
    {
        unsafe { libc::_exit(142) };
    }
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(context.executable.fd(), &mut metadata) } != 0 {
        child_send(protocol[1], LINK_COUNT_UNPROVABLE);
    }
    let link_count = stat_link_count(&metadata);
    if link_count == 0 {
        child_send(protocol[1], UNLINKED_TARGET);
    }
    if link_count > 1 {
        child_send(protocol[1], MULTIPLE_HARD_LINKS);
    }
    if metadata.st_dev != context.executable.device
        || metadata.st_ino != context.executable.inode
        || link_count != context.executable.link_count
        || metadata.st_size < 0
        || metadata.st_size as u64 != context.executable.file_size_bytes
        || metadata.st_mode != context.executable.unix_mode
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
    child_send(protocol[1], AUTHORIZED);
}

fn stat_link_count(metadata: &libc::stat) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        u64::from(metadata.st_nlink)
    }
    #[cfg(target_arch = "x86_64")]
    {
        metadata.st_nlink
    }
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _status_result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(140) }
}

fn child_send(fd: libc::c_int, value: u8) -> ! {
    let result = super::probe::write_byte(fd, value);
    unsafe { libc::_exit(if result.is_ok() { 0 } else { 143 }) }
}

fn receive_authorization(
    fd: libc::c_int,
    deadline: Instant,
) -> Result<TargetAuthorization, RuntimeFingerprintProduceError> {
    let stage = super::RuntimeObservationStage::TargetAuthorization;
    super::probe::wait_readable(fd, deadline, stage)?;
    let mut frame = [0_u8; 1];
    super::probe::receive_exact_frame(fd, &mut frame, stage)?;
    match frame[0] {
        AUTHORIZED => Ok(TargetAuthorization::Authorized),
        RESOLVED_TARGET_REPOSITORY => Ok(TargetAuthorization::ResolvedTargetRepository),
        BOUNDARY_UNPROVABLE => Ok(TargetAuthorization::BoundaryUnprovable),
        LINK_COUNT_UNPROVABLE => Ok(TargetAuthorization::LinkCountUnprovable),
        UNLINKED_TARGET => Ok(TargetAuthorization::UnlinkedTarget),
        MULTIPLE_HARD_LINKS => Ok(TargetAuthorization::MultipleHardLinks),
        IDENTITY_CHANGED => Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable),
        _ => Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::TargetAuthorization,
            reason: RuntimeObservationProtocolReason::SurplusFields,
        }),
    }
}

fn close_all(gate: [libc::c_int; 2], status: [libc::c_int; 2], protocol: [libc::c_int; 2]) {
    super::probe::close_pipe_pair(gate);
    super::probe::close_pipe_pair(status);
    super::probe::close_pipe_pair(protocol);
}
