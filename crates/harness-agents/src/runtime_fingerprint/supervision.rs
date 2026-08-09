//! Post-exec syscall supervision and bounded output capture.

use super::environment::RuntimeTermination;
use super::syscall_guard::{self, SyscallStop};
use super::target::{StoppedTarget, TargetEvent};
use super::RuntimeFingerprintProduceError;
use harness_core::stack::fingerprint::{
    RuntimeProbeFailure, RuntimeProbeFailureDetail, RuntimeProbeFailureKind,
};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SupervisionOutcome {
    Captured {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        termination: RuntimeTermination,
    },
    Failed(RuntimeProbeFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraceState {
    InitialExecExit,
    Entry,
    Exit,
    Termination,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StopDecision {
    Resume,
    Denied(RuntimeProbeFailureDetail),
}

impl TraceState {
    fn accept_syscall(&mut self, stop: SyscallStop) -> Result<StopDecision, ()> {
        match (*self, stop) {
            (Self::InitialExecExit, SyscallStop::Exit) => {
                *self = Self::Entry;
                Ok(StopDecision::Resume)
            }
            (Self::Entry, SyscallStop::Entry { number, arguments }) => {
                if let Some(detail) = syscall_guard::denied_class(number, arguments)? {
                    return Ok(StopDecision::Denied(detail));
                }
                *self = if syscall_guard::is_exit_syscall(number) {
                    Self::Termination
                } else {
                    Self::Exit
                };
                Ok(StopDecision::Resume)
            }
            (Self::Exit, SyscallStop::Exit) => {
                *self = Self::Entry;
                Ok(StopDecision::Resume)
            }
            _ => Err(()),
        }
    }

    const fn permits_signal_delivery(self) -> bool {
        matches!(self, Self::Entry)
    }

    const fn permits_exit(self) -> bool {
        matches!(self, Self::Termination)
    }

    const fn permits_signal_termination(self, signal: libc::c_int) -> bool {
        matches!(self, Self::Entry)
            || signal == libc::SIGKILL && matches!(self, Self::Exit | Self::Termination)
    }
}

struct OutputCapture {
    stdout_fd: libc::c_int,
    stderr_fd: libc::c_int,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    limit: usize,
    _descriptor_lease: super::registry::DescriptorLease,
}

impl OutputCapture {
    fn new(
        stdout_fd: libc::c_int,
        stderr_fd: libc::c_int,
        limit: usize,
        descriptor_lease: super::registry::DescriptorLease,
    ) -> Self {
        Self {
            stdout_fd,
            stderr_fd,
            stdout: Vec::new(),
            stderr: Vec::new(),
            limit,
            _descriptor_lease: descriptor_lease,
        }
    }

    fn drain_available(&mut self) -> Result<(), CaptureFailure> {
        drain_stream(
            &mut self.stdout_fd,
            &mut self.stdout,
            self.stderr.len(),
            self.limit,
        )?;
        drain_stream(
            &mut self.stderr_fd,
            &mut self.stderr,
            self.stdout.len(),
            self.limit,
        )
    }

    fn complete(mut self, deadline: Instant) -> Result<(Vec<u8>, Vec<u8>), CaptureFailure> {
        while self.stdout_fd >= 0 || self.stderr_fd >= 0 {
            self.drain_available()?;
            if self.stdout_fd < 0 && self.stderr_fd < 0 {
                break;
            }
            if Instant::now() >= deadline {
                self.close();
                return Err(CaptureFailure::Timeout);
            }
            std::thread::yield_now();
        }
        Ok((
            std::mem::take(&mut self.stdout),
            std::mem::take(&mut self.stderr),
        ))
    }

    fn close(&mut self) {
        super::probe::close_fd(self.stdout_fd);
        super::probe::close_fd(self.stderr_fd);
        self.stdout_fd = -1;
        self.stderr_fd = -1;
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureFailure {
    Limit,
    Read,
    Timeout,
}

pub(super) fn run(
    target: StoppedTarget,
    max_output_bytes: usize,
    deadline: Instant,
    stop_requested: &std::sync::atomic::AtomicBool,
) -> Result<SupervisionOutcome, RuntimeFingerprintProduceError> {
    let StoppedTarget {
        child,
        stdout,
        stderr,
        descriptor_lease,
    } = target;
    let pid = child.pid();
    let pidfd = child.pidfd();
    let mut capture = OutputCapture::new(stdout, stderr, max_output_bytes, descriptor_lease);
    let mut state = TraceState::InitialExecExit;
    if !resume_syscall(pid, 0) {
        return verification_failure(child, capture);
    }

    loop {
        if super::probe::ensure_owner_running(stop_requested).is_err() {
            return verification_failure(child, capture);
        }
        if let Err(failure) = capture.drain_available() {
            return semantic_cleanup(child, capture, capture_failure(failure, max_output_bytes)?);
        }
        if Instant::now() >= deadline {
            return semantic_cleanup(
                child,
                capture,
                RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout)?,
            );
        }
        let event = match super::target::wait_event(pidfd, pid, deadline, Some(stop_requested)) {
            Ok(event) => event,
            Err(_) if Instant::now() >= deadline => {
                return semantic_cleanup(
                    child,
                    capture,
                    RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout)?,
                );
            }
            Err(_) => return verification_failure(child, capture),
        };
        match event {
            TargetEvent::Stopped(signal) if signal == libc::SIGTRAP | 0x80 => {
                let Some(stop) = syscall_guard::read_syscall_stop(pid) else {
                    return verification_failure(child, capture);
                };
                match state.accept_syscall(stop) {
                    Ok(StopDecision::Resume) => {
                        if !resume_syscall(pid, 0) {
                            return verification_failure(child, capture);
                        }
                    }
                    Ok(StopDecision::Denied(detail)) => {
                        let failure = RuntimeProbeFailure::with_detail(
                            RuntimeProbeFailureKind::TransitiveExecutionDenied,
                            detail,
                        )?;
                        return semantic_cleanup(child, capture, failure);
                    }
                    Err(()) => return verification_failure(child, capture),
                }
            }
            TargetEvent::Stopped(signal) => {
                if !state.permits_signal_delivery() || !valid_signal_delivery_stop(pid, signal) {
                    return verification_failure(child, capture);
                }
                if !resume_syscall(pid, signal) {
                    return verification_failure(child, capture);
                }
            }
            TargetEvent::Exited(code) => {
                if !state.permits_exit() {
                    close_reaped(child, &mut capture)?;
                    return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
                }
                child.reaped()?;
                let (stdout, stderr) = match capture.complete(deadline) {
                    Ok(output) => output,
                    Err(failure) => {
                        return Ok(SupervisionOutcome::Failed(capture_failure(
                            failure,
                            max_output_bytes,
                        )?));
                    }
                };
                return Ok(SupervisionOutcome::Captured {
                    stdout,
                    stderr,
                    termination: RuntimeTermination::Exit(code),
                });
            }
            TargetEvent::Signalled(signal) => {
                if !state.permits_signal_termination(signal) {
                    close_reaped(child, &mut capture)?;
                    return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
                }
                child.reaped()?;
                let (stdout, stderr) = match capture.complete(deadline) {
                    Ok(output) => output,
                    Err(failure) => {
                        return Ok(SupervisionOutcome::Failed(capture_failure(
                            failure,
                            max_output_bytes,
                        )?));
                    }
                };
                return Ok(SupervisionOutcome::Captured {
                    stdout,
                    stderr,
                    termination: RuntimeTermination::Signal,
                });
            }
        }
    }
}

fn resume_syscall(pid: libc::pid_t, signal: libc::c_int) -> bool {
    (unsafe { libc::ptrace(libc::PTRACE_SYSCALL, pid, 0, signal) }) == 0
}

fn valid_signal_delivery_stop(pid: libc::pid_t, signal: libc::c_int) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    (unsafe { libc::ptrace(libc::PTRACE_GETSIGINFO, pid, 0, &mut info) }) == 0
        && info.si_signo == signal
        && signal_code_is_delivery(info.si_code)
}

const fn signal_code_is_delivery(code: libc::c_int) -> bool {
    code <= 0 || code >> 8 == 0
}

fn drain_stream(
    fd: &mut libc::c_int,
    destination: &mut Vec<u8>,
    other_len: usize,
    limit: usize,
) -> Result<(), CaptureFailure> {
    if *fd < 0 {
        return Ok(());
    }
    let mut buffer = [0_u8; 4096];
    loop {
        let read = unsafe { libc::read(*fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read > 0 {
            let read = read as usize;
            if other_len
                .checked_add(destination.len())
                .and_then(|total| total.checked_add(read))
                .is_none_or(|total| total > limit)
            {
                return Err(CaptureFailure::Limit);
            }
            destination.extend_from_slice(&buffer[..read]);
            continue;
        }
        if read == 0 {
            super::probe::close_fd(*fd);
            *fd = -1;
            return Ok(());
        }
        let errno = super::probe::last_errno();
        if errno == libc::EINTR {
            continue;
        }
        return if errno == libc::EAGAIN {
            Ok(())
        } else {
            Err(CaptureFailure::Read)
        };
    }
}

fn capture_failure(
    failure: CaptureFailure,
    max_output_bytes: usize,
) -> Result<RuntimeProbeFailure, RuntimeFingerprintProduceError> {
    Ok(match failure {
        CaptureFailure::Limit => RuntimeProbeFailure::with_detail(
            RuntimeProbeFailureKind::OutputLimitExceeded,
            RuntimeProbeFailureDetail::OutputLimitBytes(max_output_bytes as u64),
        )?,
        CaptureFailure::Read => {
            RuntimeProbeFailure::new(RuntimeProbeFailureKind::OutputReadFailed)?
        }
        CaptureFailure::Timeout => RuntimeProbeFailure::new(RuntimeProbeFailureKind::Timeout)?,
    })
}

fn semantic_cleanup(
    child: super::registry::RegisteredChild,
    mut capture: OutputCapture,
    failure: RuntimeProbeFailure,
) -> Result<SupervisionOutcome, RuntimeFingerprintProduceError> {
    capture.close();
    child.cleanup(Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE)?;
    Ok(SupervisionOutcome::Failed(failure))
}

fn verification_failure(
    child: super::registry::RegisteredChild,
    mut capture: OutputCapture,
) -> Result<SupervisionOutcome, RuntimeFingerprintProduceError> {
    capture.close();
    child.cleanup(Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE)?;
    Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
}

fn close_reaped(
    child: super::registry::RegisteredChild,
    capture: &mut OutputCapture,
) -> Result<(), RuntimeFingerprintProduceError> {
    child.reaped()?;
    capture.close();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe_with(contents: &[u8]) -> libc::c_int {
        let mut pipe = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK,) },
            0
        );
        assert_eq!(
            unsafe { libc::write(pipe[1], contents.as_ptr().cast(), contents.len()) },
            contents.len() as isize
        );
        super::super::probe::close_fd(pipe[1]);
        pipe[0]
    }

    #[test]
    fn output_capture_enforces_one_exact_combined_bound() {
        let registry = super::super::registry::OwnerRegistry::new();
        let capture = OutputCapture::new(
            pipe_with(b"abc"),
            pipe_with(b"de"),
            5,
            registry.reserve_descriptors(2).unwrap(),
        );
        assert_eq!(
            capture.complete(Instant::now() + std::time::Duration::from_secs(1)),
            Ok((b"abc".to_vec(), b"de".to_vec()))
        );

        let capture = OutputCapture::new(
            pipe_with(b"abc"),
            pipe_with(b"de"),
            4,
            registry.reserve_descriptors(2).unwrap(),
        );
        assert_eq!(
            capture.complete(Instant::now() + std::time::Duration::from_secs(1)),
            Err(CaptureFailure::Limit)
        );
    }

    #[test]
    fn signal_delivery_code_excludes_ptrace_events_without_rejecting_user_sources() {
        assert!(signal_code_is_delivery(libc::SI_USER));
        assert!(signal_code_is_delivery(libc::SI_QUEUE));
        assert!(signal_code_is_delivery(1));
        assert!(!signal_code_is_delivery(
            libc::SIGTRAP | (libc::PTRACE_EVENT_EXEC << 8)
        ));
    }

    #[test]
    fn trace_state_requires_initial_exec_exit_and_alternation() {
        let mut state = TraceState::InitialExecExit;
        assert_eq!(
            state.accept_syscall(SyscallStop::Exit),
            Ok(StopDecision::Resume)
        );
        assert_eq!(state, TraceState::Entry);
        assert_eq!(
            state.accept_syscall(SyscallStop::Entry {
                number: 0,
                arguments: [0; 6],
            }),
            Ok(StopDecision::Resume)
        );
        assert_eq!(state, TraceState::Exit);
        assert_eq!(
            state.accept_syscall(SyscallStop::Exit),
            Ok(StopDecision::Resume)
        );
        assert_eq!(state, TraceState::Entry);
        assert_eq!(state.accept_syscall(SyscallStop::Exit), Err(()));
    }

    #[test]
    fn trace_state_denies_before_advancing_and_closes_exit_transition() {
        let mut state = TraceState::Entry;
        let denied_number = if cfg!(target_arch = "x86_64") {
            56
        } else {
            220
        };
        assert_eq!(
            state.accept_syscall(SyscallStop::Entry {
                number: denied_number,
                arguments: [0; 6],
            }),
            Ok(StopDecision::Denied(
                RuntimeProbeFailureDetail::ProcessCreation
            ))
        );
        assert_eq!(state, TraceState::Entry);

        let exit_number = if cfg!(target_arch = "x86_64") { 60 } else { 93 };
        assert_eq!(
            state.accept_syscall(SyscallStop::Entry {
                number: exit_number,
                arguments: [0; 6],
            }),
            Ok(StopDecision::Resume)
        );
        assert_eq!(state, TraceState::Termination);
        assert!(state.permits_exit());
        assert!(!state.permits_signal_delivery());
    }
}
