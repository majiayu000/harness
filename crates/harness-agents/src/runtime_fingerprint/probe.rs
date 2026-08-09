//! Linux exact-child supervision for runtime version probes.

use super::environment::SelectedEnvironment;
use super::executable::PreparedCommand;
use super::{
    ConfiguredRuntimeExecutable, ContainmentUnavailableReason, RuntimeFingerprintOptions,
    RuntimeFingerprintProduceError,
};
use harness_core::stack::fingerprint::AgentStackFingerprintEnvelope;
use harness_core::stack::fingerprint::{RuntimeProbeFailure, RuntimeProbeFailureKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub(super) fn owner_run(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    environment: SelectedEnvironment,
    command: PreparedCommand,
    stop_requested: &AtomicBool,
) -> Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError> {
    if environment.facts.is_empty()
        || executable.executable().as_os_str().is_empty()
        || (command.candidates.is_empty() && !command.path_unusable)
    {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    if stop_requested.load(Ordering::Acquire) {
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::OwnerStopJoinTimeout,
        ));
    }
    pidfd_self_preflight()?;
    let deadline = Instant::now() + super::RUNTIME_FINGERPRINT_PROBE_DEADLINE;
    capability_child(deadline)?;
    let working_directory =
        super::executable::observe_working_directory(options.working_dir(), deadline)?;
    if working_directory.fd() < 0 || working_directory.identity_digest.as_str().is_empty() {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    if command.path_unusable {
        let failure = RuntimeProbeFailure::new(RuntimeProbeFailureKind::PathUnusable)?;
        return super::finish_runtime_envelope(
            executable,
            super::RuntimeEnvelopeEvidence {
                command_form: command.command_form,
                configured_command_digest: command.configured_command_digest,
                working_directory_digest: command.working_directory_digest,
                working_directory_identity_digest: working_directory.identity_digest.clone(),
                resolution_attempts: Vec::new(),
                executable: None,
                version: None,
                environment: environment.facts,
                failures: vec![failure],
            },
        );
    }
    Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
        ContainmentUnavailableReason::PostExecGuardUnavailable,
    ))
}

pub(super) const CHILD_READY: u8 = 1;
pub(super) const CHILD_SIGNAL_FAILED: u8 = 2;
pub(super) const CHILD_DESCRIPTOR_UNAVAILABLE: u8 = 3;
pub(super) const CHILD_DESCRIPTOR_FAILED: u8 = 4;
pub(super) const CHILD_GO: u8 = 5;

fn capability_child(deadline: Instant) -> Result<(), RuntimeFingerprintProduceError> {
    let role =
        super::RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::CapabilityCheck);
    let mut gate = [-1; 2];
    let mut status = [-1; 2];
    // SAFETY: both arrays contain space for exactly two descriptors.
    if unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(registration_error(
            role,
            super::RuntimeChildRegistrationStage::GateCreate,
        ));
    }
    // SAFETY: both arrays contain space for exactly two descriptors.
    if unsafe { libc::pipe2(status.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        close_fd(gate[0]);
        close_fd(gate[1]);
        return Err(registration_error(
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
        close_pipe_pair(gate);
        close_pipe_pair(status);
        return Err(registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }

    // SAFETY: the owner is single-threaded with respect to child ownership; the child runs only
    // the allocation-free routine below until _exit.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_capability_main(gate, status);
    }
    // SAFETY: restore the exact calling-thread mask immediately after fork.
    let restore_result =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) };
    close_fd(gate[0]);
    close_fd(status[1]);
    if pid < 0 {
        close_fd(gate[1]);
        close_fd(status[0]);
        return Err(registration_error(
            role,
            super::RuntimeChildRegistrationStage::Fork,
        ));
    }
    if restore_result != 0 {
        close_fd(gate[1]);
        close_fd(status[0]);
        rollback_unregistered_child(pid, deadline, role)?;
        return Err(registration_error(
            role,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }

    let child_status = match read_byte_before(status[0], deadline) {
        Ok(status) => status,
        Err(_) => {
            close_fd(gate[1]);
            close_fd(status[0]);
            rollback_unregistered_child(
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
    close_fd(status[0]);
    match child_status {
        CHILD_READY => {}
        CHILD_SIGNAL_FAILED => {
            close_fd(gate[1]);
            rollback_unregistered_child(pid, deadline, role)?;
            return Err(registration_error(
                role,
                super::RuntimeChildRegistrationStage::SignalIsolation,
            ));
        }
        CHILD_DESCRIPTOR_UNAVAILABLE => {
            close_fd(gate[1]);
            rollback_unregistered_child(pid, deadline, role)?;
            return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::DescriptorIsolationUnavailable,
            ));
        }
        _ => {
            close_fd(gate[1]);
            rollback_unregistered_child(pid, deadline, role)?;
            return Err(registration_error(
                role,
                super::RuntimeChildRegistrationStage::DescriptorIsolation,
            ));
        }
    }

    // SAFETY: pid is the live, still-gated direct child and flags are frozen to zero.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    if pidfd < 0 {
        close_fd(gate[1]);
        rollback_unregistered_child(pid, deadline, role)?;
        return Err(registration_error(
            role,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    if validate_ptrace_capability(pid, pidfd, gate[1], deadline).is_err() {
        close_fd(gate[1]);
        cleanup_registered_child(pidfd, deadline, role)?;
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PostExecGuardUnavailable,
        ));
    }
    close_fd(gate[1]);
    validate_and_consume_capability_exit(pidfd, pid, deadline, role)
}

fn validate_ptrace_capability(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    gate: libc::c_int,
    deadline: Instant,
) -> Result<(), ()> {
    let options = libc::PTRACE_O_TRACEEXEC | libc::PTRACE_O_TRACESYSGOOD;
    // SAFETY: pid is the registered gated child and these requests do not deliver signals.
    if unsafe { libc::ptrace(libc::PTRACE_SEIZE, pid, 0, options) } != 0
        || unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, pid, 0, 0) } != 0
        || !waitid_pidfd_stop(pidfd, pid, deadline)
    {
        return Err(());
    }
    if write_byte(gate, CHILD_GO).is_err() {
        return Err(());
    }
    // SAFETY: PTRACE_SYSCALL resumes only the registered traced child.
    if unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, 0) } != 0
        || !waitid_pidfd_stop(pidfd, pid, deadline)
    {
        return Err(());
    }
    let first_op = ptrace_syscall_info_op(pid).filter(|op| matches!(op, 1 | 2));
    // SAFETY: resume to the alternating tagged syscall stop.
    if first_op.is_none()
        || unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, 0) } != 0
        || !waitid_pidfd_stop(pidfd, pid, deadline)
    {
        return Err(());
    }
    let second_op = ptrace_syscall_info_op(pid).filter(|op| matches!(op, 1 | 2));
    if second_op.is_none()
        || first_op == second_op
        // SAFETY: detach resumes the exact traced child without injecting a signal.
        || unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) } != 0
    {
        return Err(());
    }
    Ok(())
}

fn waitid_pidfd_stop(pidfd: libc::c_int, pid: libc::pid_t, deadline: Instant) -> bool {
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

fn ptrace_syscall_info_op(pid: libc::pid_t) -> Option<u8> {
    let mut info = [0_u8; 128];
    // SAFETY: the registered child is in a ptrace syscall stop and info is a bounded output frame.
    let size = unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid,
            info.len(),
            info.as_mut_ptr(),
        )
    };
    (size > 0).then_some(info[0])
}

pub(super) fn registration_error(
    role: super::RuntimeOwnedChildRole,
    stage: super::RuntimeChildRegistrationStage,
) -> RuntimeFingerprintProduceError {
    RuntimeFingerprintProduceError::ChildRegistrationUnavailable { role, stage }
}

pub(super) fn rollback_unregistered_child(
    pid: libc::pid_t,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    // SAFETY: this exact positive PID is an unreaped direct child and has not been registered.
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        return Err(
            RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                role,
                operation: super::RuntimeChildCleanupOperation::Termination,
            },
        );
    }
    loop {
        let mut status = 0;
        // SAFETY: the same exact positive direct-child PID remains unreaped in this loop.
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

pub(super) fn cleanup_registered_child(
    pidfd: libc::c_int,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    // SAFETY: pidfd is the registered identity; no numeric PID is used after registration.
    let signal_result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if signal_result != 0 {
        close_fd(pidfd);
        return Err(
            RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                role,
                operation: super::RuntimeChildCleanupOperation::Termination,
            },
        );
    }
    let result = waitid_pidfd(pidfd, false, deadline);
    close_fd(pidfd);
    result.map(|_| ()).map_err(|_| {
        RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
            role,
            operation: super::RuntimeChildCleanupOperation::Reap,
        }
    })
}

fn validate_and_consume_capability_exit(
    pidfd: libc::c_int,
    pid: libc::pid_t,
    deadline: Instant,
    role: super::RuntimeOwnedChildRole,
) -> Result<(), RuntimeFingerprintProduceError> {
    let observed = waitid_pidfd(pidfd, true, deadline);
    if !matches!(observed, Ok((seen, libc::CLD_EXITED, 0)) if seen == pid) {
        let cleanup = bootstrap_pid_fallback(pidfd, pid, deadline, role);
        close_fd(pidfd);
        return cleanup.and(Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        )));
    }
    match waitid_pidfd(pidfd, false, deadline) {
        Ok((seen, libc::CLD_EXITED, 0)) if seen == pid => {
            close_fd(pidfd);
            Ok(())
        }
        Ok(_) => {
            close_fd(pidfd);
            Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
                ContainmentUnavailableReason::PidfdUnavailable,
            ))
        }
        Err(()) => {
            let cleanup = bootstrap_pid_fallback(pidfd, pid, deadline, role);
            close_fd(pidfd);
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
    // The capability child is the sole registered-child bootstrap exception. Keep pidfd held.
    if pidfd < 0 {
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        ));
    }
    // SAFETY: pid is the exact capability direct child and cannot be reused before reap.
    let signal_result = unsafe { libc::kill(pid, libc::SIGKILL) };
    if signal_result != 0 && last_errno() != libc::ESRCH {
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

pub(super) fn waitid_pidfd(
    pidfd: libc::c_int,
    nowait: bool,
    deadline: Instant,
) -> Result<(libc::pid_t, libc::c_int, libc::c_int), ()> {
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let options = libc::WEXITED | libc::WNOHANG | if nowait { libc::WNOWAIT } else { 0 };
        // SAFETY: pidfd remains live, info is writable, and wait is bounded with WNOHANG.
        let result =
            unsafe { libc::waitid(libc::P_PIDFD, pidfd as libc::id_t, &mut info, options) };
        if result != 0 {
            return Err(());
        }
        // SAFETY: waitid initialized the siginfo union for SIGCHLD when si_pid is nonzero.
        let seen = unsafe { info.si_pid() };
        if seen != 0 {
            return Ok((seen, info.si_code, unsafe { info.si_status() }));
        }
        if Instant::now() >= deadline {
            return Err(());
        }
        std::thread::yield_now();
    }
}

fn child_capability_main(gate: [libc::c_int; 2], status: [libc::c_int; 2]) -> ! {
    if !child_reset_signal_dispositions() {
        child_write_status_and_exit(status[1], CHILD_SIGNAL_FAILED);
    }
    let descriptor_status = child_isolate_descriptors(gate[0], status[1]);
    if descriptor_status != CHILD_READY {
        child_write_status_and_exit(status[1], descriptor_status);
    }
    // SAFETY: these two inherited foreign ends are outside the allowlist and must be closed.
    let foreign_gate = unsafe { libc::fcntl(gate[1], libc::F_GETFD) };
    let foreign_status = unsafe { libc::fcntl(status[0], libc::F_GETFD) };
    if foreign_gate != -1 || foreign_status != -1 || last_errno() != libc::EBADF {
        child_write_status_and_exit(status[1], CHILD_DESCRIPTOR_FAILED);
    }
    let empty: u64 = 0;
    // SAFETY: Linux x86_64/aarch64 use one u64 kernel signal set for rt_sigprocmask.
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
        child_write_status_and_exit(status[1], CHILD_SIGNAL_FAILED);
    }
    if write_byte(status[1], CHILD_READY).is_err() {
        // SAFETY: no Rust destructors may run in the post-fork child.
        unsafe { libc::_exit(112) };
    }
    let mut go = 0_u8;
    // SAFETY: gate[0] is the sole live gate read descriptor and points to one writable byte.
    let read = unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) };
    close_fd(gate[0]);
    close_fd(status[1]);
    // SAFETY: the capability child performs no workload beyond the validated gate.
    unsafe { libc::_exit(if read == 1 && go == CHILD_GO { 0 } else { 113 }) }
}

pub(super) fn child_reset_signal_dispositions() -> bool {
    #[repr(C)]
    struct KernelSigaction {
        handler: usize,
        flags: libc::c_ulong,
        restorer: usize,
        mask: u64,
    }
    let action = KernelSigaction {
        handler: libc::SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    let mut signal = 1;
    while signal < 65 {
        if signal != libc::SIGKILL && signal != libc::SIGSTOP {
            // SAFETY: supported Linux architectures use this frozen kernel sigaction layout and
            // one-u64 kernel signal set; raw invocation also covers glibc-reserved signals 32/33.
            if unsafe {
                libc::syscall(
                    libc::SYS_rt_sigaction,
                    signal,
                    &action,
                    std::ptr::null_mut::<KernelSigaction>(),
                    std::mem::size_of::<u64>(),
                )
            } != 0
            {
                return false;
            }
        }
        signal += 1;
    }
    true
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
            let errno = last_errno();
            return if matches!(
                errno,
                libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EINVAL
            ) {
                CHILD_DESCRIPTOR_UNAVAILABLE
            } else {
                CHILD_DESCRIPTOR_FAILED
            };
        }
    }
    CHILD_READY
}

fn child_write_status_and_exit(fd: libc::c_int, status: u8) -> ! {
    let _result = write_byte(fd, status);
    close_fd(fd);
    // SAFETY: no Rust destructors may run in the post-fork child.
    unsafe { libc::_exit(111) }
}

pub(super) fn read_byte_before(
    fd: libc::c_int,
    deadline: Instant,
) -> Result<u8, super::RuntimeChildCleanupOperation> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(super::RuntimeChildCleanupOperation::GateClose);
        }
        let remaining = deadline.saturating_duration_since(now).as_millis();
        let timeout = remaining.min(i32::MAX as u128) as libc::c_int;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pollfd points to one initialized entry and timeout is bounded.
        let poll_result = unsafe { libc::poll(&mut pollfd, 1, timeout) };
        if poll_result == 0 {
            return Err(super::RuntimeChildCleanupOperation::GateClose);
        }
        if poll_result < 0 {
            if last_errno() == libc::EINTR {
                continue;
            }
            return Err(super::RuntimeChildCleanupOperation::GateClose);
        }
        let mut value = 0_u8;
        // SAFETY: value is writable and fd is the live status descriptor.
        let read = unsafe { libc::read(fd, (&mut value as *mut u8).cast(), 1) };
        return (read == 1)
            .then_some(value)
            .ok_or(super::RuntimeChildCleanupOperation::GateClose);
    }
}

pub(super) fn write_byte(fd: libc::c_int, value: u8) -> Result<(), ()> {
    loop {
        // SAFETY: value points to one initialized byte and fd is caller-owned.
        let written = unsafe { libc::write(fd, (&value as *const u8).cast(), 1) };
        if written == 1 {
            return Ok(());
        }
        if written < 0 && last_errno() == libc::EINTR {
            continue;
        }
        return Err(());
    }
}

pub(super) fn close_pipe_pair(pair: [libc::c_int; 2]) {
    close_fd(pair[0]);
    close_fd(pair[1]);
}

pub(super) fn close_fd(fd: libc::c_int) {
    if fd >= 0 {
        // SAFETY: callers transfer each owned descriptor to this close site at most once.
        unsafe { libc::close(fd) };
    }
}

pub(super) fn last_errno() -> libc::c_int {
    // SAFETY: errno is thread-local and read immediately after a failing libc call.
    unsafe { *libc::__errno_location() }
}

fn pidfd_self_preflight() -> Result<(), RuntimeFingerprintProduceError> {
    // SAFETY: both syscalls use the current process identity and an owned descriptor.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as libc::c_int };
    if pidfd < 0 {
        return Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        ));
    }
    // SAFETY: signal zero performs an identity/permission check and does not deliver a signal.
    let signal_result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    // SAFETY: pidfd is a live descriptor created above and is closed exactly once here.
    let close_result = unsafe { libc::close(pidfd) };
    if signal_result != 0 || close_result != 0 {
        Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::PidfdUnavailable,
        ))
    } else {
        Ok(())
    }
}
