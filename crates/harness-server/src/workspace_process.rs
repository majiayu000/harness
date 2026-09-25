use std::ffi::OsStr;
use std::io;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;

const CANCEL_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct WorkspaceCommand {
    command: tokio::process::Command,
    label: &'static str,
}

impl WorkspaceCommand {
    pub(crate) fn new(program: impl AsRef<OsStr>, label: &'static str) -> Self {
        let mut command = tokio::process::Command::new(program);
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
        }
        Self { command, label }
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.command.arg(arg);
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    pub(crate) fn current_dir(&mut self, dir: impl AsRef<std::path::Path>) -> &mut Self {
        self.command.current_dir(dir);
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.command.env_remove(key);
        self
    }

    pub(crate) async fn output(&mut self) -> io::Result<Output> {
        self.command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = self.command.spawn()?;
        WorkspaceChild::new(child, self.label).output().await
    }
}

struct WorkspaceChild {
    child: Option<tokio::process::Child>,
    process_group_id: Option<u32>,
    label: &'static str,
    cleanup_disarmed: bool,
}

impl WorkspaceChild {
    fn new(child: tokio::process::Child, label: &'static str) -> Self {
        let process_group_id = child.id();
        Self {
            child: Some(child),
            process_group_id,
            label,
            cleanup_disarmed: false,
        }
    }

    async fn output(mut self) -> io::Result<Output> {
        let child = self.child.as_mut().expect("workspace child is present");
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let mut status = None;

        while stdout.is_some() || stderr.is_some() || status.is_none() {
            tokio::select! {
                result = read_pipe(&mut stdout, &mut stdout_bytes), if stdout.is_some() => {
                    if result? == 0 {
                        stdout = None;
                    }
                }
                result = read_pipe(&mut stderr, &mut stderr_bytes), if stderr.is_some() => {
                    if result? == 0 {
                        stderr = None;
                    }
                }
                result = self.child.as_mut().expect("workspace child is present").wait(), if status.is_none() => {
                    status = Some(result?);
                    self.kill_remaining_group()?;
                }
            }
        }

        self.drain_remaining_group().await?;
        self.cleanup_disarmed = true;
        Ok(Output {
            status: status.expect("workspace child exit status was observed"),
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
    }

    #[cfg(unix)]
    fn kill_remaining_group(&self) -> io::Result<()> {
        if let Some(process_group_id) = self.process_group_id {
            kill_process_group(process_group_id)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    async fn drain_remaining_group(&self) -> io::Result<()> {
        let Some(process_group_id) = self.process_group_id else {
            return Ok(());
        };
        let label = self.label;
        tokio::task::spawn_blocking(move || drain_process_group(process_group_id, label))
            .await
            .map_err(|error| io::Error::other(format!("workspace drain task failed: {error}")))?
    }

    #[cfg(not(unix))]
    async fn drain_remaining_group(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(not(unix))]
    fn kill_remaining_group(&self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for WorkspaceChild {
    fn drop(&mut self) {
        if self.cleanup_disarmed {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        let mut drain = || drain_cancelled_child(&mut child, self.process_group_id, self.label);
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(drain);
        } else {
            drain();
        }
    }
}

fn drain_cancelled_child(
    child: &mut tokio::process::Child,
    _process_group_id: Option<u32>,
    label: &'static str,
) {
    let mut child_reaped = false;

    #[cfg(unix)]
    if let Some(process_group_id) = _process_group_id {
        if let Err(error) = kill_process_group(process_group_id) {
            tracing::error!(
                process = label,
                pgid = process_group_id,
                "failed to kill cancelled workspace process group; retaining fence and retrying: {error}"
            );
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = child.start_kill() {
        tracing::error!(
            process = label,
            "failed to kill cancelled workspace process; retaining fence and retrying: {error}"
        );
    }

    loop {
        if !child_reaped {
            match child.try_wait() {
                Ok(Some(_)) => child_reaped = true,
                Ok(None) => {}
                Err(error) => tracing::error!(
                    process = label,
                    "failed to reap cancelled workspace process; retaining fence and retrying: {error}"
                ),
            }
        }

        #[cfg(unix)]
        let group_drained = match _process_group_id {
            Some(id) => match process_group_has_members(id) {
                Ok(has_members) => !has_members,
                Err(error) => {
                    tracing::error!(
                        process = label,
                        pgid = id,
                        "failed to verify cancelled workspace process group; retaining fence and retrying: {error}"
                    );
                    false
                }
            },
            None => true,
        };
        #[cfg(not(unix))]
        let group_drained = true;

        if child_reaped && group_drained {
            return;
        }

        #[cfg(unix)]
        if let Some(process_group_id) = _process_group_id {
            if let Err(error) = kill_process_group(process_group_id) {
                tracing::error!(
                    process = label,
                    pgid = process_group_id,
                    "failed to retry cancelled workspace process-group kill: {error}"
                );
            }
        }
        #[cfg(not(unix))]
        if let Err(error) = child.start_kill() {
            tracing::error!(
                process = label,
                "failed to retry cancelled workspace process kill: {error}"
            );
        }

        std::thread::sleep(CANCEL_DRAIN_POLL_INTERVAL);
    }
}

async fn read_pipe<R>(pipe: &mut Option<R>, output: &mut Vec<u8>) -> io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];
    let read = pipe
        .as_mut()
        .expect("guarded pipe read requires a pipe")
        .read(&mut buffer)
        .await?;
    output.extend_from_slice(&buffer[..read]);
    Ok(read)
}

#[cfg(unix)]
fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    let result = unsafe { posix_kill(-(process_group_id as i32), 9) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(3) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn process_group_has_members(process_group_id: u32) -> io::Result<bool> {
    let result = unsafe { posix_kill(-(process_group_id as i32), 0) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(3) {
        return Ok(false);
    }
    if error.raw_os_error() == Some(1) {
        return Ok(true);
    }
    Err(error)
}

#[cfg(unix)]
fn drain_process_group(process_group_id: u32, label: &'static str) -> io::Result<()> {
    loop {
        if !process_group_has_members(process_group_id)? {
            return Ok(());
        }
        kill_process_group(process_group_id).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to drain {label} process group {process_group_id}: {error}"),
            )
        })?;
        std::thread::sleep(CANCEL_DRAIN_POLL_INTERVAL);
    }
}

#[cfg(unix)]
unsafe fn posix_kill(pid: i32, signal: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    unsafe { kill(pid, signal) }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn root_exit_drains_closed_stdio_descendant() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let pid_path = directory.path().join("descendant.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; printf done",
            pid_path.display()
        );
        let mut command = WorkspaceCommand::new("/bin/sh", "workspace-process-test");
        command.args(["-c", &script]);
        let output = tokio::time::timeout(std::time::Duration::from_secs(3), command.output())
            .await
            .expect("background descendant must not keep output open")
            .expect("shell should run");
        assert_eq!(output.stdout, b"done");
        let descendant_pid: i32 = std::fs::read_to_string(pid_path)?.trim().parse()?;
        assert_ne!(
            unsafe { posix_kill(descendant_pid, 0) },
            0,
            "closed-stdio descendant must be gone before output returns"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_kills_descendant_group() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let root_pid_path = directory.path().join("root.pid");
        let pid_path = directory.path().join("descendant.pid");
        let script = format!(
            "echo $$ > '{}'; sleep 30 & echo $! > '{}'; wait",
            root_pid_path.display(),
            pid_path.display()
        );
        let mut command = WorkspaceCommand::new("/bin/sh", "workspace-process-test");
        command.args(["-c", &script]);
        let task = tokio::spawn(async move { command.output().await });

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !root_pid_path.exists() || !pid_path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        let root_pid: i32 = std::fs::read_to_string(&root_pid_path)?.trim().parse()?;
        let descendant_pid: i32 = std::fs::read_to_string(&pid_path)?.trim().parse()?;
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => {}
            Err(error) => return Err(error.into()),
            Ok(result) => anyhow::bail!(
                "workspace command completed before cancellation: {:?}",
                result?.status
            ),
        }
        assert_ne!(
            unsafe { posix_kill(root_pid, 0) },
            0,
            "root process must be reaped before cancellation returns"
        );
        assert_ne!(
            unsafe { posix_kill(descendant_pid, 0) },
            0,
            "descendant must be gone before cancellation returns"
        );
        assert!(
            !process_group_has_members(root_pid as u32)?,
            "process group must be drained before cancellation returns"
        );
        Ok(())
    }
}
