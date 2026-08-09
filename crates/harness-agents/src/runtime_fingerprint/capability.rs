//! Linux containment capability validation before runtime observation.

use super::{ContainmentUnavailableReason, RuntimeFingerprintProduceError};
use harness_core::stack::fingerprint::RuntimeProbeFailureDetail;
use std::time::Instant;

const MAX_CAPABILITY_SYSCALL_STOPS: usize = 16;
const PROC_SELF_MEM: &[u8; 15] = b"/proc/self/mem\0";

pub(super) fn validate(deadline: Instant) -> Result<(), RuntimeFingerprintProduceError> {
    let role =
        super::RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::CapabilityCheck);
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

    let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut saved = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    // SAFETY: initialized signal sets are confined to this owner thread.
    let mask_result = unsafe {
        libc::sigfillset(&mut blocked);
        libc::sigdelset(&mut blocked, libc::SIGKILL);
        libc::sigdelset(&mut blocked, libc::SIGSTOP);
        libc::pthread_sigmask(libc::SIG_SETMASK, &blocked, &mut saved)
    };
    if mask_result != 0 {
        super::probe::close_pipe_pair(gate);
        super::probe::close_pipe_pair(status);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }

    // SAFETY: the child executes only the allocation-free routine below until _exit.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_main(gate, status);
    }
    // SAFETY: restore the exact calling-thread mask immediately after fork.
    let restore_result =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) };
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    if pid < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::Fork,
        ));
    }
    if restore_result != 0 {
        super::probe::close_fd(gate[1]);
        super::probe::close_fd(status[0]);
        super::probe::rollback_unregistered_child(pid, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }

    let child_status = match super::probe::read_byte_before(status[0], deadline) {
        Ok(status) => status,
        Err(_) => {
            super::probe::close_fd(gate[1]);
            super::probe::close_fd(status[0]);
            super::probe::rollback_unregistered_child(
                pid,
                Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE,
                role,
            )?;
            return Err(
                RuntimeFingerprintProduceError::ObservationDeadlineExceeded {
                    stage: super::RuntimeObservationStage::CapabilityCheck,
                },
            );
        }
    };
    super::probe::close_fd(status[0]);
    match child_status {
        super::probe::CHILD_READY => {}
        super::probe::CHILD_SIGNAL_FAILED => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(pid, deadline, role)?;
            return Err(super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::SignalIsolation,
            ));
        }
        super::probe::CHILD_DESCRIPTOR_UNAVAILABLE => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(pid, deadline, role)?;
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::DescriptorIsolationUnavailable,
            ));
        }
        _ => {
            super::probe::close_fd(gate[1]);
            super::probe::rollback_unregistered_child(pid, deadline, role)?;
            return Err(super::probe::registration_error(
                role,
                super::RuntimeChildRegistrationStage::DescriptorIsolation,
            ));
        }
    }

    // SAFETY: pid is the live, still-gated direct child and flags are frozen to zero.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    if pidfd < 0 {
        super::probe::close_fd(gate[1]);
        super::probe::rollback_unregistered_child(pid, deadline, role)?;
        return Err(super::probe::registration_error(
            role,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    if validate_ptrace(pid, pidfd, gate[1], deadline).is_err() {
        super::probe::close_fd(gate[1]);
        super::probe::cleanup_registered_child(pidfd, deadline, role)?;
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PostExecGuardUnavailable,
        ));
    }
    super::probe::close_fd(gate[1]);
    validate_and_consume_exit(pidfd, pid, deadline, role)
}

fn validate_ptrace(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    gate: libc::c_int,
    deadline: Instant,
) -> Result<(), ()> {
    let options = libc::PTRACE_O_TRACEEXEC | libc::PTRACE_O_TRACESYSGOOD;
    // SAFETY: pid is the registered gated child and these requests do not deliver signals.
    if unsafe { libc::ptrace(libc::PTRACE_SEIZE, pid, 0, options) } != 0
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
                // SAFETY: the child is stopped at the suppressed syscall exit.
                return (unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) } == 0)
                    .then_some(())
                    .ok_or(());
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
    const NT_ARM_SYSTEM_CALL: usize = 0x404;
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
    pidfd: libc::c_int,
    pid: libc::pid_t,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    let observed = super::probe::waitid_pidfd(pidfd, true, deadline);
    if !matches!(observed, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup = bootstrap_pid_fallback(pidfd, pid, deadline, role);
        super::probe::close_fd(pidfd);
        return cleanup.and(Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        )));
    }
    match super::probe::waitid_pidfd(pidfd, false, deadline) {
        Ok((seen, libc::CLD_EXITED, 0)) if seen == pid => {
            super::probe::close_fd(pidfd);
            Ok(())
        }
        Ok(_) => {
            super::probe::close_fd(pidfd);
            Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::PidfdUnavailable,
            ))
        }
        Err(()) => {
            let cleanup = bootstrap_pid_fallback(pidfd, pid, deadline, role);
            super::probe::close_fd(pidfd);
            cleanup.and(Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::PidfdUnavailable,
            )))
        }
    }
}

fn bootstrap_pid_fallback(
    pidfd: libc::c_int,
    pid: libc::pid_t,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
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

fn child_main(gate: [libc::c_int; 2], status: [libc::c_int; 2]) -> ! {
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(status[1], super::probe::CHILD_SIGNAL_FAILED);
    }
    let descriptor_status = child_isolate_descriptors(gate[0], status[1]);
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
    // SAFETY: success requires the owner-suppressed mutation syscall to return ENOSYS.
    unsafe { libc::_exit(if denied { 0 } else { 114 }) }
}

fn child_isolate_descriptors(first: libc::c_int, second: libc::c_int) -> u8 {
    let low = first.min(second) as u32;
    let high = first.max(second) as u32;
    let ranges = [
        (0_u32, low.saturating_sub(1), low != 0),
        (low + 1, high.saturating_sub(1), high > low + 1),
        (high + 1, u32::MAX, high != u32::MAX),
    ];
    for (start, end, required) in ranges {
        if !required {
            continue;
        }
        // SAFETY: close_range affects only the fork child and excludes both allowlisted fds.
        if unsafe { libc::syscall(libc::SYS_close_range, start, end, 0) } != 0 {
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
    }
    super::probe::CHILD_READY
}

fn child_status_exit(fd: libc::c_int, status: u8) -> ! {
    let _result = super::probe::write_byte(fd, status);
    super::probe::close_fd(fd);
    // SAFETY: no Rust destructors may run in the post-fork child.
    unsafe { libc::_exit(111) }
}
