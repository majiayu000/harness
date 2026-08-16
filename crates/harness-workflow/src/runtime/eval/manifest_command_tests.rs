use super::*;
use crate::runtime::EvalTrustedVerifier;

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

#[test]
fn trusted_verifier_replaces_agent_visible_verify_commands() {
    let input = r#"
suite = "trusted-verifier"

[[cases]]
case_id = "gh1454-scoped-ci-jobs"
repo = "majiayu000/harness"
issue = 1454
base_commit = "9c0099ad458e82fd377fd20a8e288a46722762ef"
"#;
    let manifest = parse_benchmark_manifest_str(input).expect("trusted verifier should parse");
    let case = &manifest.cases[0];

    assert!(case.verify_commands.is_empty());
    assert_eq!(
        case.verification_command_argv()
            .expect("trusted verifier should produce governed argv"),
        vec![EvalTrustedVerifier::Gh1454CiContractV1.validation_argv()]
    );
}

#[test]
fn trusted_verifier_rejects_agent_visible_verify_commands() {
    let input = r#"
suite = "trusted-verifier"

[[cases]]
case_id = "gh1454-scoped-ci-jobs"
repo = "majiayu000/harness"
issue = 1454
base_commit = "9c0099ad458e82fd377fd20a8e288a46722762ef"
verify_commands = ["python3 inspect_the_contract.py"]
"#;
    let error = parse_benchmark_manifest_str(input)
        .expect_err("trusted verifier must not share its contract with the agent");

    assert!(error.to_string().contains("cannot expose verify_commands"));
}
