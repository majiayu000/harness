use super::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn tmp() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write_file(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

fn run(root: &Path) -> AgentStackCapabilityExtraction {
    extract_repository_capability_evidence(&AgentStackCapabilityExtractionOptions::new(
        root.to_path_buf(),
    ))
    .expect("extraction succeeds")
}

fn capability_rows(
    extraction: &AgentStackCapabilityExtraction,
) -> Vec<(
    String,
    AgentStackCapability,
    String,
    AgentStackCapabilityExtractionConfidence,
)> {
    extraction
        .evidence()
        .iter()
        .map(|item| {
            (
                item.component().source().locator().as_str().to_owned(),
                item.capability(),
                item.rule_id().to_owned(),
                item.confidence(),
            )
        })
        .collect()
}

#[test]
fn capability_extraction_reads_mcp_policy_and_hook_declarations() {
    let dir = tmp();
    write_file(
        dir.path(),
        ".mcp.json",
        br#"{
  "tools": [{
    "name": "remote_shell",
    "inputSchema": {
      "type": "object",
      "properties": {
        "command": { "type": "string" },
        "endpoint_url": { "type": "string", "format": "uri" },
        "api_key": { "type": "string" },
        "output_path": { "type": "string" }
      }
    }
  }]
}"#,
    );
    write_file(
        dir.path(),
        "requirements.toml",
        br#"[rules]
[[rules.prefix_rules]]
pattern = [{ token = "rm" }, { token = "-rf" }]
decision = "prompt"
justification = "destructive cleanup"
"#,
    );
    write_file(
        dir.path(),
        ".harness/guards/preflight.sh",
        b"#!/bin/sh\n# harness-reason: guard opens a local shell\n# harness-capabilities: shell\n",
    );

    let extraction = run(dir.path());
    assert!(
        extraction.failures().is_empty(),
        "{:#?}",
        extraction.failures()
    );
    let rows = capability_rows(&extraction);

    for expected in [
        (
            ".mcp.json",
            AgentStackCapability::Shell,
            "mcp.input_schema",
            AgentStackCapabilityExtractionConfidence::Medium,
        ),
        (
            ".mcp.json",
            AgentStackCapability::Network,
            "mcp.input_schema",
            AgentStackCapabilityExtractionConfidence::Medium,
        ),
        (
            ".mcp.json",
            AgentStackCapability::SecretRead,
            "mcp.input_schema",
            AgentStackCapabilityExtractionConfidence::Medium,
        ),
        (
            ".mcp.json",
            AgentStackCapability::FileWrite,
            "mcp.input_schema",
            AgentStackCapabilityExtractionConfidence::Medium,
        ),
        (
            "requirements.toml",
            AgentStackCapability::Destructive,
            "policy.prefix_rule",
            AgentStackCapabilityExtractionConfidence::Medium,
        ),
        (
            ".harness/guards/preflight.sh",
            AgentStackCapability::Shell,
            "hook.metadata_capabilities",
            AgentStackCapabilityExtractionConfidence::High,
        ),
    ] {
        assert!(
            rows.iter()
                .any(|(locator, capability, rule_id, confidence)| {
                    locator == expected.0
                        && *capability == expected.1
                        && rule_id == expected.2
                        && *confidence == expected.3
                }),
            "missing {expected:?} in {rows:#?}"
        );
    }
    assert!(extraction
        .evidence()
        .iter()
        .all(|item| !item.reason().trim().is_empty()));
}

#[test]
fn explicit_typed_declarations_prevent_lower_confidence_static_duplicates() {
    let dir = tmp();
    write_file(
        dir.path(),
        ".harness/guards/pre-push.sh",
        b"#!/bin/sh\n# harness-capabilities: network\ncurl https://example.invalid\n",
    );

    let extraction = run(dir.path());
    let rows = capability_rows(&extraction);

    let network_rows = rows
        .iter()
        .filter(|(_, capability, _, _)| *capability == AgentStackCapability::Network)
        .collect::<Vec<_>>();
    assert_eq!(network_rows.len(), 1, "{rows:#?}");
    assert_eq!(network_rows[0].2, "hook.metadata_capabilities");
    assert_eq!(
        network_rows[0].3,
        AgentStackCapabilityExtractionConfidence::High
    );
}

#[test]
fn binary_generated_ignored_and_out_of_scope_sources_are_not_extracted() {
    let dir = tmp();
    let outside = tmp();
    write_file(
        outside.path(),
        "outside.toml",
        b"capabilities = [\"destructive\"]\n",
    );
    write_file(
        dir.path(),
        "harness.toml",
        format!(
            "[rules]\ndiscovery_paths = [\"{}\"]\n",
            outside.path().join("outside.toml").display()
        )
        .as_bytes(),
    );
    write_file(
        dir.path(),
        ".harness/guards/binary.sh",
        b"#!/bin/sh\n# harness-capabilities: shell\n\xff\xfe",
    );
    write_file(
        dir.path(),
        ".harness/generated/policy.toml",
        b"capabilities = [\"network\"]\n",
    );
    write_file(
        dir.path(),
        "README.md",
        b"capabilities = [\"destructive\"]\n",
    );

    let extraction = run(dir.path());

    assert!(
        extraction.evidence().is_empty(),
        "{:#?}",
        extraction.evidence()
    );
    assert!(
        extraction.failures().is_empty(),
        "{:#?}",
        extraction.failures()
    );
}

#[test]
fn quoted_examples_and_markdown_documentation_do_not_emit_static_capabilities() {
    let dir = tmp();
    write_file(
        dir.path(),
        ".harness/guards/preflight.sh",
        br#"#!/bin/sh
# The destructive example below is documentation, not code:
echo "rm -rf /tmp/example"
printf 'curl https://example.invalid'
"#,
    );
    write_file(
        dir.path(),
        "rules/security.md",
        br#"# Security examples

Never run `rm -rf /`, `curl`, or `kubectl delete` from docs.
"#,
    );

    let extraction = run(dir.path());

    assert!(
        extraction.evidence().is_empty(),
        "{:#?}",
        extraction.evidence()
    );
    assert!(
        extraction.failures().is_empty(),
        "{:#?}",
        extraction.failures()
    );
}

#[test]
fn extractor_reports_parse_and_invalid_declaration_failures() {
    let dir = tmp();
    write_file(dir.path(), ".mcp.json", b"{ not valid json");
    write_file(
        dir.path(),
        ".harness/guards/preflight.sh",
        b"#!/bin/sh\n# harness-capabilities: rocket\n",
    );

    let extraction = run(dir.path());
    let failures = extraction
        .failures()
        .iter()
        .map(|failure| {
            (
                failure.component().source().locator().as_str().to_owned(),
                failure.kind(),
                failure.rule_id().map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();

    assert!(failures.contains(&(
        ".mcp.json".to_owned(),
        AgentStackCapabilityExtractionFailureKind::ParseFailed,
        Some("typed.json_parse".to_owned())
    )));
    assert!(failures.contains(&(
        ".harness/guards/preflight.sh".to_owned(),
        AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
        Some("hook.metadata_capabilities".to_owned())
    )));
}
