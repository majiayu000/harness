//! Registered initial target creation through retained-handle execveat.

use super::candidate::RetainedExecutable;
use super::environment::SelectedEnvironment;
use super::executable::RetainedWorkingDirectory;
use super::{ConfiguredRuntimeExecutable, RuntimeFingerprintProduceError, RuntimeOwnedChildRole};
use harness_core::stack::fingerprint::RuntimeProbeFailureDetail;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

const HIGH_DESCRIPTOR_BASE: libc::c_int = 64;
const CHILD_SETUP_FAILED: u8 = 1;
const CHILD_EXEC_FAILED: u8 = 2;
const SETUP_WORKING_DIRECTORY: u8 = 1;
const SETUP_TRACE: u8 = 2;

pub(super) enum TargetStart {
    SetupFailed(RuntimeProbeFailureDetail),
    ExecFailed(libc::c_int),
    ExecStopped(StoppedTarget),
}

pub(super) struct StoppedTarget {
    pub(super) pid: libc::pid_t,
    pub(super) pidfd: libc::c_int,
    pub(super) stdout: libc::c_int,
    pub(super) stderr: libc::c_int,
}

impl StoppedTarget {
    pub(super) const fn pid(&self) -> libc::pid_t {
        self.pid
    }

    pub(super) fn terminate_without_resume(
        self,
        deadline: Instant,
    ) -> Result<(), RuntimeFingerprintProduceError> {
        super::probe::close_fd(self.stdout);
        super::probe::close_fd(self.stderr);
        super::probe::cleanup_registered_child(
            self.pidfd,
            deadline,
            RuntimeOwnedChildRole::InitialTarget,
        )
    }
}

struct TargetChildDescriptors {
    gate: libc::c_int,
    status: libc::c_int,
    pre_exec: libc::c_int,
    stdin: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
    executable: libc::c_int,
    working_directory: libc::c_int,
}

struct TargetChildContext<'a> {
    descriptors: TargetChildDescriptors,
    argv: &'a [*const libc::c_char],
    envp: &'a [*const libc::c_char],
}

pub(super) fn start_initial(
    configured: &ConfiguredRuntimeExecutable,
    environment: &SelectedEnvironment,
    working_directory: &RetainedWorkingDirectory,
    executable: &RetainedExecutable,
    deadline: Instant,
) -> Result<TargetStart, RuntimeFingerprintProduceError> {
    let argv_storage = vec![
        cstring(configured.executable().as_os_str())?,
        CString::new(configured.runtime_kind().version_args()[0])
            .map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)?,
    ];
    let argv = pointer_vector(&argv_storage);
    let environment_storage = environment
        .child_path
        .as_deref()
        .map(path_environment)
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    let envp = pointer_vector(&environment_storage);

    let [mut gate, mut status, mut pre_exec, mut stdin, mut stdout, mut stderr] =
        create_target_pipes()?;
    let child_descriptors = match duplicate_child_descriptors([
        gate[0],
        status[1],
        pre_exec[1],
        stdin[0],
        stdout[1],
        stderr[1],
        executable.fd(),
        working_directory.fd(),
    ]) {
        Ok(descriptors) => descriptors,
        Err(error) => {
            close_six(gate, status, pre_exec, stdin, stdout, stderr);
            return Err(error);
        }
    };
    for descriptor in [
        gate[0],
        status[1],
        pre_exec[1],
        stdin[0],
        stdout[1],
        stderr[1],
    ] {
        super::probe::close_fd(descriptor);
    }
    gate[0] = -1;
    status[1] = -1;
    pre_exec[1] = -1;
    stdin[0] = -1;
    stdout[1] = -1;
    stderr[1] = -1;
    super::probe::close_fd(stdin[1]);
    stdin[1] = -1;
    if let Err(error) = set_nonblocking(pre_exec[0])
        .and_then(|()| set_nonblocking(stdout[0]))
        .and_then(|()| set_nonblocking(stderr[0]))
    {
        close_child_descriptors(&child_descriptors);
        close_six(gate, status, pre_exec, stdin, stdout, stderr);
        return Err(error);
    }

    let context = TargetChildContext {
        descriptors: child_descriptors,
        argv: &argv,
        envp: &envp,
    };
    let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mut saved = unsafe { std::mem::zeroed::<libc::sigset_t>() };
    let mask_result = unsafe {
        libc::sigfillset(&mut blocked);
        libc::sigdelset(&mut blocked, libc::SIGKILL);
        libc::sigdelset(&mut blocked, libc::SIGSTOP);
        libc::pthread_sigmask(libc::SIG_SETMASK, &blocked, &mut saved)
    };
    if mask_result != 0 {
        close_child_descriptors(&context.descriptors);
        close_six(gate, status, pre_exec, stdin, stdout, stderr);
        return Err(super::probe::registration_error(
            RuntimeOwnedChildRole::InitialTarget,
            super::RuntimeChildRegistrationStage::SignalIsolation,
        ));
    }
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        child_main(&context);
    }
    let restored =
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &saved, std::ptr::null_mut()) };
    close_child_descriptors(&context.descriptors);
    super::probe::close_fd(gate[0]);
    super::probe::close_fd(status[1]);
    super::probe::close_fd(pre_exec[1]);
    super::probe::close_fd(stdout[1]);
    super::probe::close_fd(stderr[1]);
    if pid < 0 || restored != 0 {
        close_parent_descriptors(gate[1], status[0], pre_exec[0], stdout[0], stderr[0]);
        if pid > 0 {
            super::probe::rollback_unregistered_child(
                pid,
                deadline,
                RuntimeOwnedChildRole::InitialTarget,
            )?;
        }
        return Err(super::probe::registration_error(
            RuntimeOwnedChildRole::InitialTarget,
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
        close_parent_descriptors(gate[1], -1, pre_exec[0], stdout[0], stderr[0]);
        super::probe::rollback_unregistered_child(
            pid,
            deadline,
            RuntimeOwnedChildRole::InitialTarget,
        )?;
        return Err(super::probe::registration_error(
            RuntimeOwnedChildRole::InitialTarget,
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
        close_parent_descriptors(gate[1], -1, pre_exec[0], stdout[0], stderr[0]);
        super::probe::rollback_unregistered_child(
            pid,
            deadline,
            RuntimeOwnedChildRole::InitialTarget,
        )?;
        return Err(super::probe::registration_error(
            RuntimeOwnedChildRole::InitialTarget,
            super::RuntimeChildRegistrationStage::PidfdOpen,
        ));
    }
    if super::probe::write_byte(gate[1], super::probe::CHILD_GO).is_err() {
        close_parent_descriptors(gate[1], -1, pre_exec[0], stdout[0], stderr[0]);
        super::probe::cleanup_registered_child(
            pidfd,
            deadline,
            RuntimeOwnedChildRole::InitialTarget,
        )?;
        return Err(super::probe::registration_error(
            RuntimeOwnedChildRole::InitialTarget,
            super::RuntimeChildRegistrationStage::GateRelease,
        ));
    }
    super::probe::close_fd(gate[1]);
    supervise_initial_stop(pid, pidfd, pre_exec[0], stdout[0], stderr[0], deadline)
}

fn supervise_initial_stop(
    pid: libc::pid_t,
    pidfd: libc::c_int,
    pre_exec: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
    deadline: Instant,
) -> Result<TargetStart, RuntimeFingerprintProduceError> {
    let initial = match wait_event(pidfd, pid, deadline) {
        Ok(event) => event,
        Err(_) => return verification_cleanup(pidfd, pre_exec, stdout, stderr),
    };
    match initial {
        TargetEvent::Stopped(libc::SIGSTOP) => {}
        TargetEvent::Exited(_) => {
            return finish_early_exit(pidfd, pre_exec, stdout, stderr);
        }
        TargetEvent::Signalled(_) => {
            return verification_after_reap(pidfd, pre_exec, stdout, stderr);
        }
        _ => return verification_cleanup(pidfd, pre_exec, stdout, stderr),
    }
    let options = libc::PTRACE_O_TRACEEXEC | libc::PTRACE_O_TRACESYSGOOD;
    if unsafe { libc::ptrace(libc::PTRACE_SETOPTIONS, pid, 0, options) } != 0
        || unsafe { libc::ptrace(libc::PTRACE_CONT, pid, 0, 0) } != 0
    {
        return verification_cleanup(pidfd, pre_exec, stdout, stderr);
    }
    let after_exec = match wait_event(pidfd, pid, deadline) {
        Ok(event) => event,
        Err(_) => return verification_cleanup(pidfd, pre_exec, stdout, stderr),
    };
    match after_exec {
        TargetEvent::Stopped(libc::SIGTRAP) if is_exec_event(pid) => {
            super::probe::close_fd(pre_exec);
            Ok(TargetStart::ExecStopped(StoppedTarget {
                pid,
                pidfd,
                stdout,
                stderr,
            }))
        }
        TargetEvent::Exited(_) => finish_early_exit(pidfd, pre_exec, stdout, stderr),
        TargetEvent::Signalled(_) => verification_after_reap(pidfd, pre_exec, stdout, stderr),
        _ => verification_cleanup(pidfd, pre_exec, stdout, stderr),
    }
}

fn finish_early_exit(
    pidfd: libc::c_int,
    pre_exec: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
) -> Result<TargetStart, RuntimeFingerprintProduceError> {
    let frame = read_pre_exec_frame(pre_exec);
    close_parent_descriptors(-1, -1, pre_exec, stdout, stderr);
    super::probe::close_fd(pidfd);
    match frame {
        Some([CHILD_SETUP_FAILED, SETUP_WORKING_DIRECTORY, _, _, _]) => Ok(
            TargetStart::SetupFailed(RuntimeProbeFailureDetail::WorkingDirectoryEnter),
        ),
        Some([CHILD_SETUP_FAILED, SETUP_TRACE, _, _, _]) => Ok(TargetStart::SetupFailed(
            RuntimeProbeFailureDetail::TraceSetup,
        )),
        Some([CHILD_EXEC_FAILED, a, b, c, d]) => {
            Ok(TargetStart::ExecFailed(i32::from_be_bytes([a, b, c, d])))
        }
        _ => Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable),
    }
}

fn verification_cleanup(
    pidfd: libc::c_int,
    pre_exec: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
) -> Result<TargetStart, RuntimeFingerprintProduceError> {
    close_parent_descriptors(-1, -1, pre_exec, stdout, stderr);
    let deadline = Instant::now() + super::RUNTIME_FINGERPRINT_CLEANUP_DEADLINE;
    super::probe::cleanup_registered_child(pidfd, deadline, RuntimeOwnedChildRole::InitialTarget)?;
    Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
}

fn verification_after_reap(
    pidfd: libc::c_int,
    pre_exec: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
) -> Result<TargetStart, RuntimeFingerprintProduceError> {
    close_parent_descriptors(-1, -1, pre_exec, stdout, stderr);
    super::probe::close_fd(pidfd);
    Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
}

pub(super) enum TargetEvent {
    Stopped(libc::c_int),
    Exited(libc::c_int),
    Signalled(libc::c_int),
}

pub(super) fn wait_event(
    pidfd: libc::c_int,
    pid: libc::pid_t,
    deadline: Instant,
) -> Result<TargetEvent, RuntimeFingerprintProduceError> {
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pidfd as libc::id_t,
                &mut info,
                libc::WSTOPPED | libc::WEXITED | libc::WNOHANG,
            )
        } != 0
        {
            return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
        }
        let seen = unsafe { info.si_pid() };
        if seen != 0 {
            if seen != pid {
                return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
            }
            return match info.si_code {
                libc::CLD_TRAPPED | libc::CLD_STOPPED => {
                    Ok(TargetEvent::Stopped(unsafe { info.si_status() }))
                }
                libc::CLD_EXITED => Ok(TargetEvent::Exited(unsafe { info.si_status() })),
                libc::CLD_KILLED | libc::CLD_DUMPED => {
                    Ok(TargetEvent::Signalled(unsafe { info.si_status() }))
                }
                _ => Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable),
            };
        }
        if Instant::now() >= deadline {
            return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
        }
        std::thread::yield_now();
    }
}

fn is_exec_event(pid: libc::pid_t) -> bool {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    (unsafe { libc::ptrace(libc::PTRACE_GETSIGINFO, pid, 0, &mut info) }) == 0
        && info.si_signo == libc::SIGTRAP
        && info.si_code == libc::SIGTRAP | (libc::PTRACE_EVENT_EXEC << 8)
}

fn child_main(context: &TargetChildContext<'_>) -> ! {
    let descriptors = &context.descriptors;
    if !super::probe::child_reset_signal_dispositions() {
        child_status_exit(descriptors.status, super::probe::CHILD_SIGNAL_FAILED);
    }
    let isolation = child_isolate_many([
        descriptors.gate,
        descriptors.status,
        descriptors.pre_exec,
        descriptors.stdin,
        descriptors.stdout,
        descriptors.stderr,
        descriptors.executable,
        descriptors.working_directory,
    ]);
    if isolation != super::probe::CHILD_READY {
        child_status_exit(descriptors.status, isolation);
    }
    if !super::candidate::child_install_empty_mask() {
        child_status_exit(descriptors.status, super::probe::CHILD_SIGNAL_FAILED);
    }
    if super::probe::write_byte(descriptors.status, super::probe::CHILD_READY).is_err() {
        unsafe { libc::_exit(161) };
    }
    let mut go = 0_u8;
    if unsafe { libc::read(descriptors.gate, (&mut go as *mut u8).cast(), 1) } != 1
        || go != super::probe::CHILD_GO
    {
        unsafe { libc::_exit(162) };
    }
    if !child_map_descriptors(descriptors) {
        child_write_frame(descriptors.pre_exec, CHILD_SETUP_FAILED, SETUP_TRACE as i32);
    }
    if unsafe { libc::fchdir(descriptors.working_directory) } != 0 {
        child_write_frame(
            descriptors.pre_exec,
            CHILD_SETUP_FAILED,
            SETUP_WORKING_DIRECTORY as i32,
        );
    }
    if unsafe { libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) } != 0 {
        child_write_frame(descriptors.pre_exec, CHILD_SETUP_FAILED, SETUP_TRACE as i32);
    }
    let pid = unsafe { libc::getpid() };
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
    if unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGSTOP) } != 0 {
        child_write_frame(descriptors.pre_exec, CHILD_SETUP_FAILED, SETUP_TRACE as i32);
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_execveat,
            super::RUNTIME_FINGERPRINT_TARGET_EXEC_FD,
            c"".as_ptr(),
            context.argv.as_ptr(),
            context.envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    let errno = if result == -1 {
        super::probe::last_errno()
    } else {
        libc::EIO
    };
    child_write_frame(descriptors.pre_exec, CHILD_EXEC_FAILED, errno);
}

fn child_map_descriptors(descriptors: &TargetChildDescriptors) -> bool {
    if unsafe {
        libc::dup3(
            descriptors.executable,
            super::RUNTIME_FINGERPRINT_TARGET_EXEC_FD,
            libc::O_CLOEXEC,
        )
    } < 0
        || unsafe { libc::dup2(descriptors.stdin, libc::STDIN_FILENO) } < 0
        || unsafe { libc::dup2(descriptors.stdout, libc::STDOUT_FILENO) } < 0
        || unsafe { libc::dup2(descriptors.stderr, libc::STDERR_FILENO) } < 0
    {
        return false;
    }
    for descriptor in [
        descriptors.gate,
        descriptors.status,
        descriptors.stdin,
        descriptors.stdout,
        descriptors.stderr,
        descriptors.executable,
    ] {
        super::probe::close_fd(descriptor);
    }
    true
}

fn child_isolate_many(mut allowed: [libc::c_int; 8]) -> u8 {
    allowed.sort_unstable();
    let mut start = 0_u32;
    for descriptor in allowed {
        let descriptor = descriptor as u32;
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

fn child_status_exit(fd: libc::c_int, value: u8) -> ! {
    let _result = super::probe::write_byte(fd, value);
    unsafe { libc::_exit(160) }
}

fn child_write_frame(fd: libc::c_int, kind: u8, value: libc::c_int) -> ! {
    let mut frame = [0_u8; 5];
    frame[0] = kind;
    frame[1..].copy_from_slice(&value.to_be_bytes());
    let written = unsafe { libc::write(fd, frame.as_ptr().cast(), frame.len()) };
    unsafe {
        libc::_exit(if written == frame.len() as isize {
            1
        } else {
            163
        })
    }
}

fn duplicate_child_descriptors(
    sources: [libc::c_int; 8],
) -> Result<TargetChildDescriptors, RuntimeFingerprintProduceError> {
    let mut duplicated = [-1; 8];
    for (index, source) in sources.into_iter().enumerate() {
        duplicated[index] =
            unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, HIGH_DESCRIPTOR_BASE) };
        if duplicated[index] < 0 {
            for descriptor in duplicated {
                super::probe::close_fd(descriptor);
            }
            return Err(super::probe::registration_error(
                RuntimeOwnedChildRole::InitialTarget,
                super::RuntimeChildRegistrationStage::DescriptorIsolation,
            ));
        }
    }
    Ok(TargetChildDescriptors {
        gate: duplicated[0],
        status: duplicated[1],
        pre_exec: duplicated[2],
        stdin: duplicated[3],
        stdout: duplicated[4],
        stderr: duplicated[5],
        executable: duplicated[6],
        working_directory: duplicated[7],
    })
}

fn create_target_pipes() -> Result<[[libc::c_int; 2]; 6], RuntimeFingerprintProduceError> {
    let mut pipes = [[-1; 2]; 6];
    for index in 0..pipes.len() {
        if unsafe { libc::pipe2(pipes[index].as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            for pipe in pipes {
                super::probe::close_pipe_pair(pipe);
            }
            return Err(super::probe::registration_error(
                RuntimeOwnedChildRole::InitialTarget,
                super::RuntimeChildRegistrationStage::GateCreate,
            ));
        }
    }
    Ok(pipes)
}

fn set_nonblocking(fd: libc::c_int) -> Result<(), RuntimeFingerprintProduceError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable)
    } else {
        Ok(())
    }
}

fn read_pre_exec_frame(fd: libc::c_int) -> Option<[u8; 5]> {
    let mut frame = [0_u8; 5];
    let read = unsafe { libc::read(fd, frame.as_mut_ptr().cast(), frame.len()) };
    (read == frame.len() as isize).then_some(frame)
}

fn cstring(value: &OsStr) -> Result<CString, RuntimeFingerprintProduceError> {
    CString::new(value.as_bytes()).map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)
}

fn path_environment(path: &OsStr) -> Result<CString, RuntimeFingerprintProduceError> {
    let mut value = b"PATH=".to_vec();
    value.extend_from_slice(path.as_bytes());
    CString::new(value).map_err(|_| RuntimeFingerprintProduceError::InvalidLaunchContext)
}

fn pointer_vector(values: &[CString]) -> Vec<*const libc::c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn close_child_descriptors(descriptors: &TargetChildDescriptors) {
    for descriptor in [
        descriptors.gate,
        descriptors.status,
        descriptors.pre_exec,
        descriptors.stdin,
        descriptors.stdout,
        descriptors.stderr,
        descriptors.executable,
        descriptors.working_directory,
    ] {
        super::probe::close_fd(descriptor);
    }
}

fn close_parent_descriptors(
    gate: libc::c_int,
    status: libc::c_int,
    pre_exec: libc::c_int,
    stdout: libc::c_int,
    stderr: libc::c_int,
) {
    for descriptor in [gate, status, pre_exec, stdout, stderr] {
        super::probe::close_fd(descriptor);
    }
}

fn close_six(
    gate: [libc::c_int; 2],
    status: [libc::c_int; 2],
    pre_exec: [libc::c_int; 2],
    stdin: [libc::c_int; 2],
    stdout: [libc::c_int; 2],
    stderr: [libc::c_int; 2],
) {
    for pair in [gate, status, pre_exec, stdin, stdout, stderr] {
        super::probe::close_pipe_pair(pair);
    }
}
