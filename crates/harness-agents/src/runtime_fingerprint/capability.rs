//! Linux containment capability validation before runtime observation.

use super::{ContainmentUnavailableReason, RuntimeFingerprintProduceError};
use harness_core::stack::fingerprint::RuntimeProbeFailureDetail;
use std::time::Instant;

const MAX_CAPABILITY_SYSCALL_STOPS: usize = 16;
const PROC_SELF_MEM: &[u8; 15] = b"/proc/self/mem\0";
const CAPABILITY_ARG0: &[u8; 19] = b"harness-capability\0";
const LOAD_OFFSET: usize = 0x1000;
const LOAD_ADDRESS: u64 = 0x0040_0000;

pub(super) fn validate(
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<(), RuntimeFingerprintProduceError> {
    let role =
        super::RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::CapabilityCheck);
    let pidfd_lease = registry.reserve_child_pidfd(role)?;
    let mut descriptor_lease = registry.reserve_descriptors(5)?;
    let trusted_image = trusted_capability_image();
    let executable = create_capability_executable(&trusted_image, descriptor_lease.split_off(1)?)?;
    let mut gate = [-1; 2];
    let mut status = [-1; 2];
    // SAFETY: both arrays contain space for exactly two descriptors.
    if unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateCreate,
        ));
    }
    // SAFETY: both arrays contain space for exactly two descriptors.
    if unsafe { libc::pipe2(status.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        super::probe::close_pipe_pair(gate);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateCreate,
        ));
    }

    let saved_signal_mask = super::probe::block_all_signals().map_err(|()| {
        super::probe::close_pipe_pair(gate);
        super::probe::close_pipe_pair(status);
        super::probe::parent_signal_isolation_error()
    })?;

    // SAFETY: the child executes only the allocation-free routine below until _exit.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_main(gate, status, executable.fd());
    }
    let restore_result = super::probe::restore_signal_mask(saved_signal_mask);
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    if restore_result.is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        if pid > 0 {
            super::probe::rollback_unregistered_child(registry, pid, role)?;
        }
        return Err(super::probe::parent_signal_isolation_error());
    }
    if pid < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::Fork,
        ));
    }

    let child_status = match super::probe::read_byte_before(status[0], deadline) {
        Ok(status) => status,
        Err(error) => {
            super::probe::close_fd(gate[1]);
            super::probe::close_fd(status[0]);
            super::probe::rollback_unregistered_child(registry, pid, role)?;
            return Err(super::probe::readiness_error(role, error));
        }
    };
    super::probe::close_fd(status[0]);
    match child_status {
        super::probe::CHILD_READY => {}
        super::probe::CHILD_SIGNAL_FAILED => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(registry, pid, role)?;
            return Err(super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::SignalIsolation,
            ));
        }
        super::probe::CHILD_DESCRIPTOR_UNAVAILABLE => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(registry, pid, role)?;
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::DescriptorIsolationUnavailable,
            ));
        }
        _ => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(registry, pid, role)?;
            return Err(super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::DescriptorIsolation,
            ));
        }
    }

    // SAFETY: pid is the live, still-gated direct child and flags are frozen to zero.
    let pidfd = super::probe::open_child_pidfd(pid);
    if pidfd < 0 {
        super::probe::close_fd(gate[1]);
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
            return Err(error);
        }
    };
    if validate_ptrace(pid, child.pidfd(), gate[1], &executable, deadline, registry).is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::cleanup_registered_child(child)?;
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PostExecGuardUnavailable,
        ));
    }
    super::probe::close_fd(gate[1]);
    validate_and_consume_exit(child, pid, deadline, role)
}

fn create_capability_executable(
    image: &[u8],
    descriptor_lease: super::registry::DescriptorLease,
) -> Result<super::candidate::RetainedExecutable, RuntimeFingerprintProduceError> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"harness-capability".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        ) as libc::c_int
    };
    if fd < 0 || !write_capability_image(fd, image) {
        super::probe::close_fd(fd);
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PostExecGuardUnavailable,
        ));
    }
    match super::candidate::RetainedExecutable::from_capability_image(fd, descriptor_lease, image) {
        Ok(executable) => Ok(executable),
        Err(error) => {
            super::probe::close_fd(fd);
            Err(error)
        }
    }
}

fn write_capability_image(fd: libc::c_int, image: &[u8]) -> bool {
    let mut offset = 0_usize;
    while offset < image.len() {
        let written = unsafe {
            libc::pwrite(
                fd,
                image.as_ptr().add(offset).cast(),
                image.len() - offset,
                offset as libc::off_t,
            )
        };
        if written <= 0 {
            return false;
        }
        offset += written as usize;
    }
    (unsafe { libc::fchmod(fd, 0o500) }) == 0
        && (unsafe {
            libc::fcntl(
                fd,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        }) == 0
}

fn trusted_capability_image() -> Vec<u8> {
    let code = trusted_exit_code();
    let mut image = vec![0_u8; LOAD_OFFSET + code.len()];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    image[18..20].copy_from_slice(&native_machine().to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[24..32].copy_from_slice(&(LOAD_ADDRESS + LOAD_OFFSET as u64).to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&2_u16.to_le_bytes());
    let image_size = image.len() as u64;
    write_program_header(
        &mut image[64..120],
        1,
        5,
        0,
        LOAD_ADDRESS,
        image_size,
        0x1000,
    );
    write_program_header(&mut image[120..176], 0x6474_e551, 6, 0, 0, 0, 16);
    image[LOAD_OFFSET..].copy_from_slice(code);
    image
}

fn write_program_header(
    header: &mut [u8],
    kind: u32,
    flags: u32,
    offset: u64,
    address: u64,
    size: u64,
    alignment: u64,
) {
    header[0..4].copy_from_slice(&kind.to_le_bytes());
    header[4..8].copy_from_slice(&flags.to_le_bytes());
    header[8..16].copy_from_slice(&offset.to_le_bytes());
    header[16..24].copy_from_slice(&address.to_le_bytes());
    header[24..32].copy_from_slice(&address.to_le_bytes());
    header[32..40].copy_from_slice(&size.to_le_bytes());
    header[40..48].copy_from_slice(&size.to_le_bytes());
    header[48..56].copy_from_slice(&alignment.to_le_bytes());
}

#[cfg(target_arch = "x86_64")]
fn trusted_exit_code() -> &'static [u8] {
    &[0xb8, 0x3c, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05]
}

#[cfg(target_arch = "aarch64")]
fn trusted_exit_code() -> &'static [u8] {
    &[
        0x00, 0x00, 0x80, 0xd2, 0xa8, 0x0b, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
    ]
}

#[cfg(target_arch = "x86_64")]
const fn native_machine() -> u16 {
    62
}

#[cfg(target_arch = "aarch64")]
const fn native_machine() -> u16 {
    183
}

fn validate_ptrace(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    gate: libc::c_int,
    executable: &super::candidate::RetainedExecutable,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<(), ()> {
    // SAFETY: pid is the registered gated child and these requests do not deliver signals.
    if unsafe {
        libc::ptrace(
            libc::PTRACE_SEIZE,
            pid,
            0,
            super::probe::PTRACE_GUARD_OPTIONS,
        )
    } != 0
        || unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, pid, 0, 0) } != 0
        || !wait_for_stop(pidfd, pid, deadline)
        || super::probe::write_byte(gate, super::probe::CHILD_GO).is_err()
    {
        return Err(());
    }

    let mut previous = None;
    for _ in 0..MAX_CAPABILITY_SYSCALL_STOPS {
        let stop = resume_to_syscall_stop(pid, pidfd, deadline)?;
        if !alternates(previous, stop) {
            return Err(());
        }
        match stop {
            super::syscall_guard::SyscallStop::Entry { number, arguments }
                if is_expected_mutation(number, arguments) =>
            {
                suppress_syscall(pid)?;
                if !matches!(
                    resume_to_syscall_stop(pid, pidfd, deadline)?,
                    super::syscall_guard::SyscallStop::Exit
                ) {
                    return Err(());
                }
                return validate_exec_stop(pid, pidfd, executable, deadline, registry);
            }
            super::syscall_guard::SyscallStop::Entry { number, arguments } => {
                if !matches!(
                    super::syscall_guard::denied_class(number, arguments),
                    Ok(None)
                ) {
                    return Err(());
                }
            }
            super::syscall_guard::SyscallStop::Exit => {}
        }
        previous = Some(stop);
    }
    Err(())
}

fn validate_exec_stop(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    executable: &super::candidate::RetainedExecutable,
    deadline: Instant,
    registry: &super::registry::OwnerRegistry,
) -> Result<(), ()> {
    if unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) } != 0 {
        return Err(());
    }
    let event = super::target::wait_event(pidfd, pid, deadline, None).map_err(|_| ())?;
    if !matches!(
        event,
        super::target::TargetEvent::Stopped(status)
            if status == libc::SIGTRAP | (libc::PTRACE_EVENT_EXEC << 8)
                && super::target::is_exec_event(pid)
    ) || super::exec_stop::verify(pid, executable, deadline, registry).map_err(|_| ())?
        != super::exec_stop::ExecStopCheckpoint::Consistent
    {
        return Err(());
    }
    // SAFETY: the child remains stopped at its verified exec event and may now run the fixed image.
    (unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) } == 0)
        .then_some(())
        .ok_or(())
}

fn resume_to_syscall_stop(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    deadline: Instant,
) -> Result<super::syscall_guard::SyscallStop, ()> {
    // SAFETY: PTRACE_SYSCALL resumes only the registered traced child.
    if unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, 0) } != 0
        || !wait_for_stop(pidfd, pid, deadline)
    {
        return Err(());
    }
    super::syscall_guard::read_syscall_stop(pid).ok_or(())
}

fn alternates(
    previous: Option<super::syscall_guard::SyscallStop>,
    current: super::syscall_guard::SyscallStop,
) -> bool {
    use super::syscall_guard::SyscallStop::{Entry, Exit};
    matches!(
        (previous, current),
        (None, _) | (Some(Entry { .. }), Exit) | (Some(Exit), Entry { .. })
    )
}

fn is_expected_mutation(number: u64, arguments: [u64; 6]) -> bool {
    number == libc::SYS_openat as u64
        && arguments[2] & libc::O_ACCMODE as u64 == libc::O_RDWR as u64
        && matches!(
            super::syscall_guard::denied_class(number, arguments),
            Ok(Some(RuntimeProbeFailureDetail::ExecutableImageMutation))
        )
}

#[cfg(target_arch = "x86_64")]
fn suppress_syscall(pid: libc::pid_t) -> Result<(), ()> {
    let mut registers = unsafe { std::mem::zeroed::<libc::user_regs_struct>() };
    // SAFETY: the exact traced child is stopped at syscall entry and registers is writable.
    if unsafe { libc::ptrace(libc::PTRACE_GETREGS, pid, 0, &mut registers) } != 0 {
        return Err(());
    }
    registers.orig_rax = u64::MAX;
    // SAFETY: only the syscall-number register is changed before the entry is resumed.
    (unsafe { libc::ptrace(libc::PTRACE_SETREGS, pid, 0, &registers) } == 0)
        .then_some(())
        .ok_or(())
}

#[cfg(target_arch = "aarch64")]
fn suppress_syscall(pid: libc::pid_t) -> Result<(), ()> {
    const NT_PRSTATUS: usize = 1;
    const NT_ARM_SYSTEM_CALL: usize = 0x404;
    let mut registers = unsafe { std::mem::zeroed::<libc::user_regs_struct>() };
    let mut get_register_vector = libc::iovec {
        iov_base: (&mut registers as *mut libc::user_regs_struct).cast(),
        iov_len: std::mem::size_of::<libc::user_regs_struct>(),
    };
    // SAFETY: NT_PRSTATUS reads the complete stopped task register set.
    if unsafe {
        libc::ptrace(
            libc::PTRACE_GETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut get_register_vector,
        )
    } != 0
    {
        return Err(());
    }
    registers.regs[0] = (-(libc::ENOSYS as i64)) as u64;
    let mut set_register_vector = libc::iovec {
        iov_base: (&mut registers as *mut libc::user_regs_struct).cast(),
        iov_len: std::mem::size_of::<libc::user_regs_struct>(),
    };
    // SAFETY: arm64 requires both NO_SYSCALL and an explicit x0 result when skipping a syscall.
    if unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid,
            NT_PRSTATUS as *mut libc::c_void,
            &mut set_register_vector,
        )
    } != 0
    {
        return Err(());
    }
    let mut syscall_number: libc::c_int = -1;
    let mut vector = libc::iovec {
        iov_base: (&mut syscall_number as *mut libc::c_int).cast(),
        iov_len: std::mem::size_of::<libc::c_int>(),
    };
    // SAFETY: NT_ARM_SYSTEM_CALL writes the stopped task's dedicated syscall-number field.
    (unsafe {
        libc::ptrace(
            libc::PTRACE_SETREGSET,
            pid,
            NT_ARM_SYSTEM_CALL as *mut libc::c_void,
            &mut vector,
        )
    } == 0)
        .then_some(())
        .ok_or(())
}

fn wait_for_stop(pidfd: libc::c_int, pid: libc::pid_t, deadline: Instant) -> bool {
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: pidfd is live and the nonblocking wait writes one siginfo value.
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pidfd as libc::id_t,
                &mut info,
                libc::WSTOPPED | libc::WNOHANG,
            )
        } != 0
        {
            return false;
        }
        // SAFETY: waitid initialized SIGCHLD union fields when si_pid is nonzero.
        let seen = unsafe { info.si_pid() };
        if seen != 0 {
            return seen == pid && info.si_code == libc::CLD_TRAPPED;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::yield_now();
    }
}

fn validate_and_consume_exit(
    child: super::registry::RegisteredChild,
    pid: libc::pid_t,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    let observed = super::probe::waitid_pidfd(child.pidfd(), true, deadline);
    if !matches!(observed, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        bootstrap_pid_fallback(child.pidfd(), pid, role)?;
        child.reaped()?;
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        ));
    }
    let consumed = super::probe::waitid_pidfd(child.pidfd(), false, deadline);
    match consumed {
        Ok((seen, libc::CLD_EXITED, 0)) if seen == pid => {
            child.reaped()?;
            Ok(())
        }
        Ok(_) => {
            child.reaped()?;
            Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::PidfdUnavailable,
            ))
        }
        Err(()) => {
            bootstrap_pid_fallback(child.pidfd(), pid, role)?;
            child.reaped()?;
            Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::PidfdUnavailable,
            ))
        }
    }
}

fn bootstrap_pid_fallback(
    pidfd: libc::c_int,
    pid: libc::pid_t,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    let deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
    if pidfd < 0 {
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        ));
    }
    // SAFETY: pid is the exact capability direct child and cannot be reused before reap.
    let signal_result = unsafe { libc::kill(pid, libc::SIGKILL) };
    if signal_result != 0 && super::probe::last_errno() != libc::ESRCH {
        return Err(
            RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                role,
                operation: super::RuntimeChildCleanupOperation::Termination,
            },
        );
    }
    let mut status = 0;
    loop {
        // SAFETY: pid is the capability direct child and pidfd remains held until return.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return Ok(());
        }
        if result < 0 && super::probe::last_errno() == libc::EINTR {
            continue;
        }
        if result < 0 || Instant::now() >= deadline {
            return Err(
                RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                    role,
                    operation: super::RuntimeChildCleanupOperation::Reap,
                },
            );
        }
        std::thread::yield_now();
    }
}

fn child_main(gate: [libc::c_int; 2], status: [libc::c_int; 2], executable_fd: libc::c_int) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let descriptor_status = child_isolate_descriptors([gate[0], status[1], executable_fd]);
    if descriptor_status != super::probe::CHILD_READY {
        child_status_exit(status[1], descriptor_status);
    }
    // SAFETY: these inherited foreign ends are outside the allowlist and must be closed.
    let foreign_gate = unsafe { libc::fcntl(gate[1], libc::F_GETFD) };
    let foreign_status = unsafe { libc::fcntl(status[0], libc::F_GETFD) };
    if foreign_gate != -1 || foreign_status != -1 || super::probe::last_errno() != libc::EBADF {
        child_status_exit(status[1], super::probe::CHILD_DESCRIPTOR_FAILED);
    }
    let empty: u64 = 0;
    // SAFETY: supported Linux architectures use one u64 kernel signal set.
    let mask_result = unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            &empty,
            std::ptr::null_mut::<u64>(),
            std::mem::size_of::<u64>(),
        )
    };
    if mask_result != 0 {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(status[1], super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(112) };
    }
    let mut go = 0_u8;
    // SAFETY: gate[0] is the sole live gate read descriptor.
    let read = unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) };
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    if read != 1 || go != super::probe::CHILD_GO {
        unsafe { libc::_exit(113) };
    }
    // SAFETY: the fixed NUL-terminated path and flags form the capability mutation request.
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            PROC_SELF_MEM.as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC,
            0,
        )
    };
    let denied = opened == -1 && super::probe::last_errno() == libc::ENOSYS;
    if opened >= 0 {
        super::probe::close_fd(opened as libc::c_int);
    }
    if !denied {
        unsafe { libc::_exit(114) };
    }
    let argv: [*const libc::c_char; 2] = [CAPABILITY_ARG0.as_ptr().cast(), std::ptr::null()];
    let envp = [std::ptr::null::<libc::c_char>()];
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            executable_fd,
            c"".as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
        libc::_exit(115)
    }
}

fn child_isolate_descriptors(mut allowed: [libc::c_int; 3]) -> u8 {
    allowed.sort_unstable();
    let mut start = 0_u32;
    for descriptor in allowed {
        let descriptor = descriptor as u32;
        if start < descriptor
            && unsafe { libc::syscall(libc::SYS_close_range, start, descriptor - 1, 0) } != 0
        {
            let errno = super::probe::last_errno();
            return if matches!(
                errno,
                libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EINVAL
            ) {
                super::probe::CHILD_DESCRIPTOR_UNAVAILABLE
            } else {
                super::probe::CHILD_DESCRIPTOR_FAILED
            };
        }
        start = descriptor.saturating_add(1);
    }
    if start != 0 && unsafe { libc::syscall(libc::SYS_close_range, start, u32::MAX, 0) } != 0 {
        return if matches!(
            super::probe::last_errno(),
            libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EINVAL
        ) {
            super::probe::CHILD_DESCRIPTOR_UNAVAILABLE
        } else {
            super::probe::CHILD_DESCRIPTOR_FAILED
        };
    }
    super::probe::CHILD_READY
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _result = super::probe::write_byte(fd, status);
    super::probe::close_fd(fd);
    // SAFETY: no Rust destructors may run in the post-fork child.
    unsafe { libc::_exit(111) }
}
