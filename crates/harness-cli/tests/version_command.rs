use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn compiled_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("harness binary should spawn")
}

fn assert_version(dir: &Path, args: &[&str], expected_stdout: &str) {
    let output = run(dir, args);
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(
        output.status.success(),
        "harness {args:?} in {} failed: stdout={stdout:?} stderr={stderr:?}",
        dir.display()
    );
    assert_eq!(stdout, expected_stdout, "args={args:?}");
    assert_eq!(stderr, "", "args={args:?}");
    assert!(!stdout.contains("99.88.77"), "args={args:?}");
}

#[test]
fn version_queries_report_the_compiled_binary_version() {
    let version = compiled_version();
    let subcommand = format!("Current version: {version}\n");
    let flag = format!("harness {version}\n");

    let empty = tempfile::tempdir().expect("empty temp dir");
    let foreign = tempfile::tempdir().expect("unrelated project dir");
    fs::write(
        foreign.path().join("Cargo.toml"),
        "[package]\nname = \"unrelated\"\nversion = \"99.88.77\"\nedition = \"2021\"\n",
    )
    .expect("write unrelated Cargo.toml");
    let missing_config = empty.path().join("missing-harness.toml");
    let missing_config = missing_config.to_str().expect("temp path is utf-8");

    for dir in [empty.path(), foreign.path()] {
        assert_version(dir, &["version"], &subcommand);
        assert_version(dir, &["--version"], &flag);
        assert_version(dir, &["-V"], &flag);
        assert_version(dir, &["--config", missing_config, "version"], &subcommand);
        assert_version(dir, &["--config", missing_config, "--version"], &flag);
        assert_version(dir, &["--config", missing_config, "-V"], &flag);
    }
}
