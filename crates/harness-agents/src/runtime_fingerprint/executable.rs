//! Bounded executable resolution and observation.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use super::RuntimeFingerprintProduceError;
use harness_core::stack::Sha256Digest;
use std::ffi::OsStr;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use std::path::Path;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use std::time::Instant;

const WORKING_DIRECTORY_IDENTITY_DOMAIN: &[u8] =
    b"harness_runtime_working_directory_identity_v0_1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxElfArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxStaticElfClassification {
    Eligible,
    Unsupported,
    ExecutableTooLarge,
}

pub fn classify_static_linux_elf(
    image: &[u8],
    architecture: LinuxElfArchitecture,
) -> LinuxStaticElfClassification {
    if image.len() as u64 > super::RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES {
        return LinuxStaticElfClassification::ExecutableTooLarge;
    }
    if image.len() < 64
        || &image[..4] != b"\x7fELF"
        || image[4] != 2
        || image[5] != 1
        || image[6] != 1
        || le_u32(image, 20) != Some(1)
        || le_u16(image, 52) != Some(64)
        || le_u16(image, 54) != Some(56)
    {
        return LinuxStaticElfClassification::Unsupported;
    }
    let expected_machine = match architecture {
        LinuxElfArchitecture::X86_64 => 62,
        LinuxElfArchitecture::Aarch64 => 183,
    };
    if !matches!(le_u16(image, 16), Some(2 | 3)) || le_u16(image, 18) != Some(expected_machine) {
        return LinuxStaticElfClassification::Unsupported;
    }
    let Some(program_offset) = le_u64(image, 32).and_then(|value| usize::try_from(value).ok())
    else {
        return LinuxStaticElfClassification::Unsupported;
    };
    let Some(program_count) = le_u16(image, 56).map(usize::from) else {
        return LinuxStaticElfClassification::Unsupported;
    };
    let Some(program_bytes) = program_count.checked_mul(56) else {
        return LinuxStaticElfClassification::Unsupported;
    };
    if program_count == 0
        || program_count == 0xffff
        || program_offset
            .checked_add(program_bytes)
            .is_none_or(|end| end > image.len())
    {
        return LinuxStaticElfClassification::Unsupported;
    }
    let mut stack_headers = 0;
    for index in 0..program_count {
        let offset = program_offset + index * 56;
        let program_type = le_u32(image, offset);
        let flags = le_u32(image, offset + 4);
        if program_type == Some(3)
            || (program_type == Some(1) && flags.is_some_and(|value| value & 3 == 3))
        {
            return LinuxStaticElfClassification::Unsupported;
        }
        if program_type == Some(0x6474_e551) {
            stack_headers += 1;
            if flags.is_some_and(|value| value & 1 != 0) {
                return LinuxStaticElfClassification::Unsupported;
            }
        }
    }
    if stack_headers == 1 {
        LinuxStaticElfClassification::Eligible
    } else {
        LinuxStaticElfClassification::Unsupported
    }
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[derive(Debug)]
pub(super) struct RetainedWorkingDirectory {
    fd: libc::c_int,
    descriptor_lease: Option<super::registry::DescriptorLease>,
    pub(super) identity_digest: Sha256Digest,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl RetainedWorkingDirectory {
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

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
impl Drop for RetainedWorkingDirectory {
    fn drop(&mut self) {
        super::probe::close_fd(self.fd);
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub(super) fn observe_working_directory(
    path: &Path,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<RetainedWorkingDirectory, RuntimeFingerprintProduceError> {
    let role =
        super::RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::WorkingDirectory);
    let pidfd_lease = registry.reserve_child_pidfd(role)?;
    use std::os::unix::ffi::OsStrExt;

    let mut descriptor_lease = registry.reserve_descriptors(7)?;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let mut gate = [-1; 2];
    let mut status = [-1; 2];
    let mut protocol = [-1; 2];
    // SAFETY: all arrays contain the exact descriptor capacity required by their syscalls.
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
        super::probe::close_pipe_pair(gate);
        super::probe::close_pipe_pair(status);
        super::probe::close_pipe_pair(protocol);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateCreate,
        ));
    }
    let saved_signal_mask = super::probe::block_all_signals().map_err(|()| {
        close_observation_fds(gate, status, protocol);
        super::probe::parent_signal_isolation_error()
    })?;
    // SAFETY: only the allocation-free child routine runs after fork.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_open_working_directory(gate, status, protocol, path.as_ptr());
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
    // SAFETY: pid is the live gated direct child and flags are frozen.
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
    let observed = receive_working_directory(protocol[0], deadline);
    super::probe::close_fd(protocol[0]);
    let exited = super::probe::waitid_pidfd(child.pidfd(), false, deadline);
    if !matches!(exited, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        child.cleanup(Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE)?;
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::WorkingDirectory,
            reason: super::RuntimeObservationProtocolReason::HelperExited,
        });
    }
    child.reaped()?;
    let mut observed = observed?;
    observed.attach_descriptor_lease(descriptor_lease.split_off(1)?)?;
    Ok(observed)
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn child_open_working_directory(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
    path: *const libc::c_char,
) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let isolation = child_isolate_three(gate[0], status[1], protocol[1]);
    if isolation != super::probe::CHILD_READY {
        child_status_exit(status[1], isolation);
    }
    if !child_install_empty_mask() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(status[1], super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(121) };
    }
    let mut go = 0_u8;
    let read = unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) };
    if read != 1 || go != super::probe::CHILD_GO {
        unsafe { libc::_exit(122) };
    }
    // SAFETY: path is parent-built NUL-terminated storage retained across fork.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            path,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        ) as libc::c_int
    };
    if fd < 0 {
        child_send_cwd(protocol[1], None, 0, 0);
    }
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut metadata) } != 0 {
        child_send_cwd(protocol[1], None, 0, 0);
    }
    child_send_cwd(protocol[1], Some(fd), metadata.st_dev, metadata.st_ino);
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn close_observation_fds(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    protocol: [libc::c_int; 2],
) {
    super::probe::close_pipe_pair(gate);
    super::probe::close_pipe_pair(status);
    super::probe::close_pipe_pair(protocol);
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _status_result = super::probe::write_byte(fd, status);
    unsafe { libc::_exit(120) }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn child_install_empty_mask() -> bool {
    let empty: u64 = 0;
    // SAFETY: supported Linux architectures use a one-u64 kernel signal set.
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

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn child_isolate_three(a: libc::c_int, b: libc::c_int, c: libc::c_int) -> u8 {
    let mut allowed = [a as u32, b as u32, c as u32];
    if allowed[0] > allowed[1] {
        allowed.swap(0, 1);
    }
    if allowed[1] > allowed[2] {
        allowed.swap(1, 2);
    }
    if allowed[0] > allowed[1] {
        allowed.swap(0, 1);
    }
    let starts = [0, allowed[0] + 1, allowed[1] + 1, allowed[2] + 1];
    let ends = [
        allowed[0].saturating_sub(1),
        allowed[1].saturating_sub(1),
        allowed[2].saturating_sub(1),
        u32::MAX,
    ];
    for index in 0..4 {
        if starts[index] <= ends[index]
            && !allowed.contains(&starts[index])
            && unsafe { libc::syscall(libc::SYS_close_range, starts[index], ends[index], 0) } != 0
        {
            return if matches!(
                super::probe::last_errno(),
                libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EINVAL
            ) {
                super::probe::CHILD_DESCRIPTOR_UNAVAILABLE
            } else {
                super::probe::CHILD_DESCRIPTOR_FAILED
            };
        }
    }
    super::probe::CHILD_READY
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[repr(C, align(8))]
struct DescriptorControl([u8; 64]);

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn child_send_cwd(fd: libc::c_int, transferred: Option<libc::c_int>, dev: u64, ino: u64) -> ! {
    let mut frame = [0_u8; 17];
    frame[0] = if transferred.is_some() { 1 } else { 2 };
    frame[1..9].copy_from_slice(&dev.to_be_bytes());
    frame[9..17].copy_from_slice(&ino.to_be_bytes());
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
    unsafe { libc::_exit(if sent == frame.len() as isize { 0 } else { 123 }) }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn receive_working_directory(
    fd: libc::c_int,
    deadline: Instant,
) -> Result<RetainedWorkingDirectory, RuntimeFingerprintProduceError> {
    let stage = super::RuntimeObservationStage::WorkingDirectory;
    super::probe::wait_readable(fd, deadline, stage)?;
    let mut frame = [0_u8; 17];
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
    let received = super::probe::recvmsg_retry(fd, &mut message, libc::MSG_CMSG_CLOEXEC, stage)?;
    if let Some(reason) =
        super::probe::fixed_frame_protocol_reason(received, frame.len(), message.msg_flags)
    {
        super::probe::close_received_rights(&message);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid { stage, reason });
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if frame[0] == 2 && header.is_null() {
        if frame[1..].iter().any(|byte| *byte != 0) {
            return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
                stage,
                reason: super::RuntimeObservationProtocolReason::SurplusFields,
            });
        }
        return Err(RuntimeFingerprintProduceError::WorkingDirectoryUnavailable);
    }
    if frame[0] != 1 {
        super::probe::close_received_rights(&message);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::WorkingDirectory,
            reason: super::RuntimeObservationProtocolReason::DescriptorCountMismatch,
        });
    }
    let retained = super::probe::take_exactly_one_received_right(&message).map_err(|()| {
        RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::WorkingDirectory,
            reason: super::RuntimeObservationProtocolReason::DescriptorCountMismatch,
        }
    })?;
    let flags = unsafe { libc::fcntl(retained, libc::F_GETFL) };
    let descriptor_flags = unsafe { libc::fcntl(retained, libc::F_GETFD) };
    if flags < 0 || flags & libc::O_PATH == 0 || descriptor_flags & libc::FD_CLOEXEC == 0 {
        super::probe::close_fd(retained);
        return Err(RuntimeFingerprintProduceError::ObservationProtocolInvalid {
            stage: super::RuntimeObservationStage::WorkingDirectory,
            reason: super::RuntimeObservationProtocolReason::DescriptorCountMismatch,
        });
    }
    let mut dev_bytes = [0_u8; 8];
    let mut ino_bytes = [0_u8; 8];
    dev_bytes.copy_from_slice(&frame[1..9]);
    ino_bytes.copy_from_slice(&frame[9..17]);
    let dev = u64::from_be_bytes(dev_bytes);
    let ino = u64::from_be_bytes(ino_bytes);
    Ok(RetainedWorkingDirectory {
        fd: retained,
        descriptor_lease: None,
        identity_digest: runtime_working_directory_identity_digest(dev, ino),
    })
}

pub fn runtime_working_directory_identity_digest(device: u64, inode: u64) -> Sha256Digest {
    let mut framed = Vec::with_capacity(WORKING_DIRECTORY_IDENTITY_DOMAIN.len() + 16);
    framed.extend_from_slice(WORKING_DIRECTORY_IDENTITY_DOMAIN);
    framed.extend_from_slice(&device.to_be_bytes());
    framed.extend_from_slice(&inode.to_be_bytes());
    Sha256Digest::from_bytes(&framed)
}

pub(super) fn native_os_units_len(value: &OsStr, limit: usize) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().iter().take(limit + 1).count()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value.encode_wide().take(limit + 1).count()
    }
}
