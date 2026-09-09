use super::*;

#[test]
fn eval_manifest_normalizes_trusted_network_allowlist() {
    let input = r#"
suite = "harness-core"

[isolation]
network_allowlist = [" GitHub.COM. ", "api.github.com"]

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
"#;

    let manifest = parse_benchmark_manifest_str(input).expect("allowlisted case should parse");

    assert_eq!(
        manifest.cases[0].isolation.network_allowlist,
        vec!["github.com".to_string(), "api.github.com".to_string()]
    );
}

#[test]
fn eval_manifest_rejects_ambiguous_network_allowlist_entries() {
    let input = r#"
suite = "harness-core"

[isolation]
network_allowlist = ["*.github.com"]

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
"#;

    let err = parse_benchmark_manifest_str(input)
        .expect_err("ambiguous eval network allowlist should fail");

    assert!(err.to_string().contains("network_allowlist"));
}
