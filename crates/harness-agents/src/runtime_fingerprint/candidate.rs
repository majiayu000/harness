//! Handle-based Linux executable candidate observation.

use super::executable::{CandidateReference, ResolvedCandidate, RetainedWorkingDirectory};
use super::{RuntimeFingerprintProduceError, RuntimeObservationProtocolReason};
use harness_core::stack::fingerprint::RuntimeProbeFailureKind;
use harness_core::stack::Sha256Digest;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

const FRAME_BYTES: usize = 69;
const STATUS_RETAINED: u8 = 1;
const STATUS_ABSENT: u8 = 2;
const STATUS_NOT_REGULAR: u8 = 3;
const STATUS_NOT_EXECUTABLE: u8 = 4;
const STATUS_OPEN_FAILED: u8 = 5;
const STATUS_METADATA_UNAVAILABLE: u8 = 6;
const STATUS_EXECUTABLE_TOO_LARGE: u8 = 7;
const STATUS_READ_FAILED: u8 = 8;
const STATUS_UNSUPPORTED_FORMAT: u8 = 9;

#[derive(Debug)]
pub(super) struct RetainedExecutable {
    fd: libc::c_int,
    descriptor_lease: Option<super::registry::DescriptorLease>,
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) link_count: u64,
    pub(super) file_size_bytes: u64,
    pub(super) unix_mode: u32,
    pub(super) executable_sha256: Sha256Digest,
}

impl RetainedExecutable {
    pub(super) const fn fd(&self) -> libc::c_int {
        self.fd
    }

    fn attach_descriptor_lease(
        &mut self,
        lease: super::registry::DescriptorLease,
    ) -> Result<(), RuntimeFingerprintProduceError> {
        if self.descriptor_lease.is_some() {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        self.descriptor_lease = Some(lease);
        Ok(())
    }
}

impl Drop for RetainedExecutable {
    fn drop(&mut self) {
        super::probe::close_fd(self.fd);
    }
}

#[derive(Debug)]
pub(super) enum CandidateObservation {
    Absent,
    NotRegular,
    NotExecutable,
    UnsupportedFormat,
    InspectionFailed(RuntimeProbeFailureKind),
    Retained(RetainedExecutable),
}

pub(super) fn observe_candidate(
    candidate: &ResolvedCandidate,
    working_directory: &RetainedWorkingDirectory,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<CandidateObservation, RuntimeFingerprintProduceError> {
    let mut descriptor_lease = registry.reserve_descriptors(7)?;
    let (directory_fd, path) = match &candidate.reference {
        CandidateReference::Absolute(path) => (libc::AT_FDCWD, path),
        CandidateReference::WorkingDirectoryRelative(path) => (working_directory.fd(), path),
    };
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let role = super::RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::Candidate);
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
        child_observe_candidate(gate, status, protocol, directory_fd, path.as_ptr());
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
    let observed = receive_candidate(protocol[0], deadline);
    super::probe::close_fd(protocol[0]);
    let exited = super::probe::waitid_pidfd(child.pidfd(), false, deadline);
    if !matches!(exited, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup_deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
        child.cleanup(cleanup_deadline)?;
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::HelperExited,
        });
    }
    child.reaped()?;
    match observed {
        Ok(CandidateObservation::Retained(mut executable)) => {
            executable.attach_descriptor_lease(descriptor_lease.split_off(1)?)?;
            Ok(CandidateObservation::Retained(executable))
        }
        other => other,
    }
}

fn child_observe_candidate(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
    directory_fd: libc::c_int,
    path: *const libc::c_char,
) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let retained_directory = (directory_fd != libc::AT_FDCWD).then_some(directory_fd);
    let isolation = child_isolate(gate[0], status[1], protocol[1], [retained_directory, None]);
    if isolation != super::probe::CHILD_READY {
        child_status_exit(status[1], isolation);
    }
    if !child_install_empty_mask() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(status[1], super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(131) };
    }
    let mut go = 0_u8;
    if unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) } != 1
        || go != super::probe::CHILD_GO
    {
        unsafe { libc::_exit(132) };
    }

    let mut preliminary = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstatat(directory_fd, path, &mut preliminary, 0) } == 0 {
        if preliminary.st_mode & libc::S_IFMT != libc::S_IFREG {
            child_send(protocol[1], STATUS_NOT_REGULAR, None, None);
        }
        if preliminary.st_mode & 0o111 == 0 {
            child_send(protocol[1], STATUS_NOT_EXECUTABLE, None, None);
        }
    } else if matches!(super::probe::last_errno(), libc::ENOENT | libc::ENOTDIR) {
        child_send(protocol[1], STATUS_ABSENT, None, None);
    }

    let fd = unsafe {
        libc::openat(
            directory_fd,
            path,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        let code = if matches!(super::probe::last_errno(), libc::ENOENT | libc::ENOTDIR) {
            STATUS_ABSENT
        } else {
            STATUS_OPEN_FAILED
        };
        child_send(protocol[1], code, None, None);
    }
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut metadata) } != 0 {
        child_send(protocol[1], STATUS_METADATA_UNAVAILABLE, None, None);
    }
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        child_send(protocol[1], STATUS_NOT_REGULAR, None, None);
    }
    if metadata.st_mode & 0o111 == 0 {
        child_send(protocol[1], STATUS_NOT_EXECUTABLE, None, None);
    }
    if metadata.st_size < 0
        || metadata.st_size as u64 > super::RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES
    {
        child_send(protocol[1], STATUS_EXECUTABLE_TOO_LARGE, None, None);
    }
    let digest = match child_hash(fd) {
        Ok(digest) => digest,
        Err(status) => child_send(protocol[1], status, None, None),
    };
    if !child_static_elf_is_supported(fd) {
        child_send(protocol[1], STATUS_UNSUPPORTED_FORMAT, None, None);
    }
    child_send(
        protocol[1],
        STATUS_RETAINED,
        Some(fd),
        Some((&metadata, digest)),
    );
}

fn child_hash(fd: libc::c_int) -> Result<[u8; 32], u8> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 32 * 1024];
    let mut offset = 0_u64;
    loop {
        if offset > super::RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES {
            return Err(STATUS_EXECUTABLE_TOO_LARGE);
        }
        let remaining = super::RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES + 1 - offset;
        let requested = buffer.len().min(remaining as usize);
        let read = unsafe {
            libc::pread(
                fd,
                buffer.as_mut_ptr().cast(),
                requested,
                offset as libc::off_t,
            )
        };
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        if read < 0 {
            if super::probe::last_errno() == libc::EINTR {
                continue;
            }
            return Err(STATUS_READ_FAILED);
        }
        let read = read as usize;
        hasher.update(&buffer[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or(STATUS_EXECUTABLE_TOO_LARGE)?;
    }
}

pub(super) fn child_checkpoint_hash(fd: libc::c_int) -> Option<[u8; 32]> {
    child_hash(fd).ok()
}

fn child_static_elf_is_supported(fd: libc::c_int) -> bool {
    let mut header = [0_u8; 64];
    if !pread_exact(fd, &mut header, 0)
        || &header[..4] != b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || read_u32(&header, 20) != 1
        || read_u16(&header, 52) != 64
        || read_u16(&header, 54) != 56
        || !matches!(read_u16(&header, 16), 2 | 3)
    {
        return false;
    }
    let expected_machine = if cfg!(target_arch = "x86_64") {
        62
    } else if cfg!(target_arch = "aarch64") {
        183
    } else {
        return false;
    };
    if read_u16(&header, 18) != expected_machine {
        return false;
    }
    let program_offset = read_u64(&header, 32);
    let program_count = read_u16(&header, 56);
    if program_count == 0 || program_count == 0xffff {
        return false;
    }
    let mut stack_headers = 0_u16;
    let mut index = 0_u16;
    while index < program_count {
        let Some(offset) = program_offset.checked_add(u64::from(index) * 56) else {
            return false;
        };
        let mut program = [0_u8; 56];
        if !pread_exact(fd, &mut program, offset) {
            return false;
        }
        let program_type = read_u32(&program, 0);
        let flags = read_u32(&program, 4);
        if program_type == 3 || (program_type == 1 && flags & 3 == 3) {
            return false;
        }
        if program_type == 0x6474_e551 {
            stack_headers += 1;
            if flags & 1 != 0 {
                return false;
            }
        }
        index += 1;
    }
    stack_headers == 1
}

fn pread_exact(fd: libc::c_int, buffer: &mut [u8], offset: u64) -> bool {
    let mut filled = 0_usize;
    while filled < buffer.len() {
        let Some(position) = offset.checked_add(filled as u64) else {
            return false;
        };
        let read = unsafe {
            libc::pread(
                fd,
                buffer[filled..].as_mut_ptr().cast(),
                buffer.len() - filled,
                position as libc::off_t,
            )
        };
        if read == 0 {
            return false;
        }
        if read < 0 {
            if super::probe::last_errno() == libc::EINTR {
                continue;
            }
            return false;
        }
        filled += read as usize;
    }
    true
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

pub(super) fn child_install_empty_mask() -> bool {
    let empty: u64 = 0;
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            &empty,
            std::ptr::null_mut::<u64>(),
            std::mem::size_of::<u64>(),
        ) == 0
    }
}

pub(super) fn child_isolate(
    gate: libc::c_int,
    status: libc::c_int,
    protocol: libc::c_int,
    extras: [Option<libc::c_int>; 2],
) -> u8 {
    let mut allowed = [
        gate as u32,
        status as u32,
        protocol as u32,
        u32::MAX,
        u32::MAX,
    ];
    let mut count = 3;
    for descriptor in extras.into_iter().flatten() {
        allowed[count] = descriptor as u32;
        count += 1;
    }
    allowed[..count].sort_unstable();
    let mut start = 0_u32;
    for descriptor in allowed[..count].iter().copied() {
        if start < descriptor
            && unsafe { libc::syscall(libc::SYS_close_range, start, descriptor - 1, 0) } != 0
        {
            return isolation_failure();
        }
        start = descriptor.saturating_add(1);
    }
    if start != 0 && unsafe { libc::syscall(libc::SYS_close_range, start, u32::MAX, 0) } != 0 {
        return isolation_failure();
    }
    super::probe::CHILD_READY
}

fn isolation_failure() -> u8 {
    if matches!(
        super::probe::last_errno(),
        libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EINVAL
    ) {
        super::probe::CHILD_DESCRIPTOR_UNAVAILABLE
    } else {
        super::probe::CHILD_DESCRIPTOR_FAILED
    }
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _status_result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(130) }
}

#[repr(C, align(8))]
struct DescriptorControl([u8; 64]);

fn child_send(
    fd: libc::c_int,
    status: u8,
    transferred: Option<libc::c_int>,
    metadata: Option<(&libc::stat, [u8; 32])>,
) -> ! {
    let mut frame = [0_u8; FRAME_BYTES];
    frame[0] = status;
    if let Some((metadata, digest)) = metadata {
        frame[1..9].copy_from_slice(&metadata.st_dev.to_be_bytes());
        frame[9..17].copy_from_slice(&metadata.st_ino.to_be_bytes());
        frame[17..25].copy_from_slice(&stat_link_count(metadata).to_be_bytes());
        frame[25..33].copy_from_slice(&(metadata.st_size as u64).to_be_bytes());
        frame[33..37].copy_from_slice(&metadata.st_mode.to_be_bytes());
        frame[37..69].copy_from_slice(&digest);
    }
    let mut iov = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    let mut control = DescriptorControl([0; 64]);
    if let Some(transferred) = transferred {
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as _;
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<libc::c_int>(), transferred);
        }
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    unsafe { libc::_exit(if sent == frame.len() as isize { 0 } else { 133 }) }
}

pub(super) fn stat_link_count(metadata: &libc::stat) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        u64::from(metadata.st_nlink)
    }
    #[cfg(target_arch = "x86_64")]
    {
        metadata.st_nlink
    }
}

fn receive_candidate(
    fd: libc::c_int,
    deadline: Instant,
) -> Result<CandidateObservation, RuntimeFingerprintProduceError> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pollfd, 1, remaining.min(i32::MAX as u128) as _) } <= 0 {
        return Err(
            RuntimeFingerprintProduceError::ObservationDeadlineExceeded {
                stage: super::RuntimeObservationStage::Candidate,
            },
        );
    }
    let mut frame = [0_u8; FRAME_BYTES];
    let mut control = DescriptorControl([0; 64]);
    let mut iov = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen = control.0.len();
    let received = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received != frame.len() as isize
        || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
    {
        close_received_rights(&message);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::TruncatedFrame,
        });
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if frame[0] != STATUS_RETAINED {
        if !header.is_null() || frame[1..].iter().any(|byte| *byte != 0) {
            close_received_rights(&message);
            return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
                stage: super::RuntimeObservationStage::Candidate,
                reason: RuntimeObservationProtocolReason::SurplusFields,
            });
        }
        return decode_failure(frame[0]);
    }
    if header.is_null()
        || unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
        || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
        || unsafe { (*header).cmsg_len }
            != unsafe { libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) } as usize
        || !unsafe { libc::CMSG_NXTHDR(&message, header) }.is_null()
    {
        close_received_rights(&message);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::DescriptorCountMismatch,
        });
    }
    let retained =
        unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<libc::c_int>()) };
    if retained < 0 {
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::DescriptorCountMismatch,
        });
    }
    let flags = unsafe { libc::fcntl(retained, libc::F_GETFL) };
    let descriptor_flags = unsafe { libc::fcntl(retained, libc::F_GETFD) };
    if flags < 0
        || flags & libc::O_ACCMODE != libc::O_RDONLY
        || flags & libc::O_NONBLOCK == 0
        || descriptor_flags & libc::FD_CLOEXEC == 0
    {
        super::probe::close_fd(retained);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::DescriptorCountMismatch,
        });
    }
    let mut digest_text = String::with_capacity(64);
    for byte in &frame[37..69] {
        write!(&mut digest_text, "{byte:02x}")
            .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    }
    let executable_sha256 = Sha256Digest::parse(&digest_text).map_err(|_| {
        super::probe::close_fd(retained);
        RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::Candidate,
            reason: RuntimeObservationProtocolReason::SurplusFields,
        }
    })?;
    Ok(CandidateObservation::Retained(RetainedExecutable {
        fd: retained,
        descriptor_lease: None,
        device: frame_u64(&frame, 1),
        inode: frame_u64(&frame, 9),
        link_count: frame_u64(&frame, 17),
        file_size_bytes: frame_u64(&frame, 25),
        unix_mode: u32::from_be_bytes([frame[33], frame[34], frame[35], frame[36]]),
        executable_sha256,
    }))
}

fn close_received_rights(message: &libc::msghdr) {
    let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
    while !header.is_null() {
        let is_rights = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        let header_bytes = unsafe { libc::CMSG_LEN(0) } as usize;
        let data_bytes = unsafe { (*header).cmsg_len }.saturating_sub(header_bytes);
        if is_rights {
            let count = data_bytes / std::mem::size_of::<libc::c_int>();
            let mut index = 0;
            while index < count {
                let descriptor = unsafe {
                    std::ptr::read_unaligned(
                        libc::CMSG_DATA(header).cast::<libc::c_int>().add(index),
                    )
                };
                super::probe::close_fd(descriptor);
                index += 1;
            }
        }
        header = unsafe { libc::CMSG_NXTHDR(message, header) };
    }
}

fn decode_failure(status: u8) -> Result<CandidateObservation, RuntimeFingerprintProduceError> {
    let observation = match status {
        STATUS_ABSENT => CandidateObservation::Absent,
        STATUS_NOT_REGULAR => CandidateObservation::NotRegular,
        STATUS_NOT_EXECUTABLE => CandidateObservation::NotExecutable,
        STATUS_UNSUPPORTED_FORMAT => CandidateObservation::UnsupportedFormat,
        STATUS_OPEN_FAILED => {
            CandidateObservation::InspectionFailed(RuntimeProbeFailureKind::OpenFailed)
        }
        STATUS_METADATA_UNAVAILABLE => {
            CandidateObservation::InspectionFailed(RuntimeProbeFailureKind::MetadataUnavailable)
        }
        STATUS_EXECUTABLE_TOO_LARGE => {
            CandidateObservation::InspectionFailed(RuntimeProbeFailureKind::ExecutableTooLarge)
        }
        STATUS_READ_FAILED => {
            CandidateObservation::InspectionFailed(RuntimeProbeFailureKind::ReadFailed)
        }
        _ => {
            return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
                stage: super::RuntimeObservationStage::Candidate,
                reason: RuntimeObservationProtocolReason::SurplusFields,
            });
        }
    };
    Ok(observation)
}

fn frame_u64(frame: &[u8; FRAME_BYTES], offset: usize) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(&frame[offset..offset + 8]);
    u64::from_be_bytes(value)
}

fn close_all(gate: [libc::c_int; 2], status: [libc::c_int; 2], protocol: [libc::c_int; 2]) {
    super::probe::close_pipe_pair(gate);
    super::probe::close_pipe_pair(status);
    super::probe::close_pipe_pair(protocol);
}
