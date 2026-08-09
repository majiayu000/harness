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
    super::capability::validate(deadline)?;
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
    let mut disposition = super::resolution::resolve(
        executable,
        options,
        &environment,
        &command,
        &working_directory,
        deadline,
    )?;
    loop {
        let (candidate_index, candidate, retained, attempts) = match disposition {
            super::resolution::ResolutionDisposition::Complete(envelope) => return Ok(*envelope),
            super::resolution::ResolutionDisposition::Selected {
                candidate_index,
                candidate,
                executable: retained,
                attempts,
            } => (candidate_index, candidate, retained, attempts),
        };
        match super::launch::launch_initial(
            super::completion::InitialCompletion {
                configured: executable,
                options,
                environment: &environment,
                command: &command,
                working_directory: &working_directory,
                candidate: &candidate,
                executable: &retained,
            },
            attempts,
            deadline,
        )? {
            super::launch::InitialLaunch::Complete(envelope) => return Ok(*envelope),
            super::launch::InitialLaunch::ContinueAfterEacces(attempts) => {
                disposition = super::resolution::resume_after_eacces(
                    executable,
                    options,
                    &environment,
                    &command,
                    &working_directory,
                    deadline,
                    super::resolution::ResolutionCursor {
                        next_candidate_index: candidate_index + 1,
                        attempts,
                    },
                )?;
            }
        }
    }
}

pub(super) const CHILD_READY: u8 = 1;
pub(super) const CHILD_SIGNAL_FAILED: u8 = 2;
pub(super) const CHILD_DESCRIPTOR_UNAVAILABLE: u8 = 3;
pub(super) const CHILD_DESCRIPTOR_FAILED: u8 = 4;
pub(super) const CHILD_GO: u8 = 5;

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
