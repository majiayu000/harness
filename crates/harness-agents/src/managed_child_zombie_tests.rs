use super::{
    process_group_has_live_members, process_group_has_members, set_process_group, ManagedChild,
};
use std::path::Path;
use std::time::Duration;

const FIXTURE_MODE: &str = "HARNESS_MANAGED_CHILD_ZOMBIE_TEST_MODE";
const FIXTURE_ROOT: &str = "HARNESS_MANAGED_CHILD_ZOMBIE_TEST_ROOT";
const TEST_NAME: &str =
    "managed_child_zombie_tests::managed_child_does_not_wait_for_external_reaper";

#[test]
fn managed_child_does_not_wait_for_external_reaper() -> anyhow::Result<()> {
    if let Ok(mode) = harness_core::config::process_env::var(FIXTURE_MODE) {
        let root = harness_core::config::process_env::var(FIXTURE_ROOT)?;
        let mut runtime = if mode == "drop_multi_thread" {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        return runtime
            .enable_all()
            .build()?
            .block_on(run_fixture(Path::new(&root), &mode));
    }

    if !super::process_group::procfs_matches_pid_namespace() {
        eprintln!("SKIP external-reaper fixture: procfs does not describe this PID namespace");
        return Ok(());
    }

    // The Python parent adopts only this fixture's descendants. It withholds
    // waitpid until the Rust fixture returns, making the external zombie
    // deterministic without changing the test runner's subreaper state.
    let supervisor = r#"
import ctypes, os, pathlib, signal, subprocess, sys, time
root, binary, test_name, mode = sys.argv[1:]
root = pathlib.Path(root)
libc = ctypes.CDLL(None, use_errno=True)
assert libc.prctl(36, 1, 0, 0, 0) == 0
env = os.environ.copy()
env['HARNESS_MANAGED_CHILD_ZOMBIE_TEST_MODE'] = mode
env['HARNESS_MANAGED_CHILD_ZOMBIE_TEST_ROOT'] = str(root)
runner = subprocess.Popen([binary, '--exact', test_name, '--nocapture'], env=env)
descendant = None
try:
    deadline = time.monotonic() + 5
    while not (root / 'root-reaped').exists():
        assert runner.poll() is None, 'Rust fixture exited before its child was ready'
        assert time.monotonic() < deadline, 'Rust fixture did not reap its root'
        time.sleep(.01)
    pgid, descendant = map(int, (root / 'root-reaped').read_text().split())
    os.killpg(pgid, signal.SIGKILL)
    status = os.waitid(os.P_PID, descendant, os.WEXITED | os.WNOWAIT)
    assert status.si_status == signal.SIGKILL
    os.killpg(pgid, 0)
    (root / 'zombie-confirmed').write_text('exited, not reaped')
    assert runner.wait(timeout=3) == 0, 'ManagedChild failed to confirm quiescence'
    os.killpg(pgid, 0)
finally:
    if runner.poll() is None:
        runner.kill()
        runner.wait()
    if descendant is not None:
        os.waitpid(descendant, 0)
"#;
    for mode in ["drop_current_thread", "drop_multi_thread", "async_cleanup"] {
        let root = tempfile::tempdir()?;
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(supervisor)
            .arg(root.path())
            .arg(std::env::current_exe()?)
            .arg(TEST_NAME)
            .arg(mode)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{mode} external-reaper fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn run_fixture(root: &Path, mode: &str) -> anyhow::Result<()> {
    let pid_path = root.join("descendant-pid");
    let mut command = tokio::process::Command::new("python3");
    command
        .arg("-c")
        .arg(
            "import os,pathlib,sys,time; p=os.fork(); \
             pathlib.Path(sys.argv[1]).write_text(str(p)) if p else time.sleep(30)",
        )
        .arg(&pid_path)
        .kill_on_drop(true);
    set_process_group(&mut command);
    let child = command.spawn()?;
    let pgid = child.id().expect("new child has a PID");
    let mut managed = ManagedChild::new(child, "external reaper fixture");
    assert!(managed.wait().await?.success());
    assert!(
        process_group_has_live_members(pgid),
        "the descendant is still alive"
    );
    let descendant = std::fs::read_to_string(&pid_path)?;
    std::fs::write(root.join("root-reaped"), format!("{pgid} {descendant}"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.join("zombie-confirmed").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert!(
        process_group_has_members(pgid),
        "the external reaper still owns the zombie"
    );
    assert!(
        !process_group_has_live_members(pgid),
        "pidfd must acknowledge descendant exit"
    );
    let started = std::time::Instant::now();
    if mode == "async_cleanup" {
        managed.cleanup_after_child_exit().await?;
    }
    drop(managed);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        process_group_has_members(pgid),
        "cleanup must not steal the external wait status"
    );
    Ok(())
}
