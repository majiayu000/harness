use super::*;
use crate::task_runner::TaskId;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) fn git_command_std() -> std::process::Command {
    let mut cmd = std::process::Command::new(git_binary());
    for key in GIT_LOCAL_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

pub(crate) fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn async_env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn async_cwd_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) struct ScopedEnvVar {
    key: String,
    original: Option<String>,
    _guard: MutexGuard<'static, ()>,
}

impl ScopedEnvVar {
    pub(crate) fn set(key: &str, value: &str) -> Self {
        let guard = env_lock().lock().expect("env lock should not be poisoned");
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self {
            key: key.to_string(),
            original,
            _guard: guard,
        }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(value) = &self.original {
            unsafe { std::env::set_var(&self.key, value) };
        } else {
            unsafe { std::env::remove_var(&self.key) };
        }
    }
}

pub(crate) fn run_git(args: &[&str]) -> std::process::Output {
    let output = git_command_std()
        .args(args)
        .output()
        .expect("git command failed to spawn");
    assert!(
        output.status.success(),
        "git command failed: args={args:?}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub(crate) async fn github_state_server(path: &'static str, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GitHub mock");
    let addr = listener.local_addr().expect("GitHub mock address");
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 2048];
        let Ok(n) = socket.read(&mut buf).await else {
            return;
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let (status, response_body) = if request.starts_with(&format!("GET {path} ")) {
            ("200 OK", body)
        } else {
            ("404 Not Found", "{}")
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    format!("http://{addr}")
}

pub(crate) fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        run_git(args);
    };
    run(&["-C", &dir.to_string_lossy(), "init"]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "config",
        "user.email",
        "test@harness.test",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "config",
        "user.name",
        "Harness Test",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
    run(&[
        "-C",
        &dir.to_string_lossy(),
        "remote",
        "add",
        "origin",
        &dir.to_string_lossy(),
    ]);
}

pub(crate) fn current_branch(repo: &Path) -> String {
    let out = run_git(&[
        "-C",
        &repo.to_string_lossy(),
        "rev-parse",
        "--abbrev-ref",
        "HEAD",
    ]);
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

pub(crate) fn test_task_id() -> TaskId {
    harness_core::types::TaskId("550e8400-e29b-41d4-a716-446655440000".to_string())
}
