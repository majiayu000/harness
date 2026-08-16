use super::*;

fn manifest_with(command_mode: Option<&str>, command: &str) -> String {
    let mode = command_mode
        .map(|mode| format!("verify_command_mode = \"{mode}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
suite = "command-mode"

[[cases]]
repo = "owner/repo"
issue = 1
base_commit = "abcdef1"
{mode}verify_commands = ["{command}"]
"#
    )
}

#[test]
fn argv_mode_rejects_implicit_shell_operators() {
    let input = manifest_with(None, "cargo test && cargo clippy");
    let error = parse_benchmark_manifest_str(&input)
        .expect_err("default command mode must reject implicit shell syntax");

    assert!(error
        .to_string()
        .contains("uses shell syntax without verify_command_mode"));
}

#[test]
fn explicit_shell_mode_produces_a_governed_shell_argv() {
    let input = manifest_with(Some("shell"), "cargo test && cargo clippy");
    let manifest = parse_benchmark_manifest_str(&input).expect("shell mode should parse");

    assert_eq!(
        manifest.cases[0]
            .verification_command_argv()
            .expect("normalized command should produce argv"),
        vec![vec![
            "bash".to_string(),
            "-lc".to_string(),
            "cargo test && cargo clippy".to_string(),
        ]]
    );
}
