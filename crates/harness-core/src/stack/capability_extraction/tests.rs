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
fn raised_file_limit_is_applied_to_inventory_and_extraction() {
    let dir = tmp();
    let padding = "x".repeat(1024 * 1024);
    write_file(
        dir.path(),
        ".mcp.json",
        format!(r#"{{"capabilities":["network"],"padding":"{padding}"}}"#).as_bytes(),
    );

    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf())
        .with_max_file_bytes(2 * 1024 * 1024)
        .expect("valid raised limit");
    let extraction = extract_repository_capability_evidence(&options)
        .expect("raised limit reaches inventory and extraction");

    assert!(capability_rows(&extraction)
        .iter()
        .any(|(locator, capability, rule_id, _)| locator == ".mcp.json"
            && *capability == AgentStackCapability::Network
            && rule_id == "mcp.explicit_capabilities"));
}

#[test]
fn changed_content_is_rejected_after_inventory() {
    let dir = tmp();
    write_file(dir.path(), ".mcp.json", br#"{"capabilities":["network"]}"#);
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf());
    let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).expect("open root");
    let inventory = inventory_with_root(&root, &options.inventory_options).expect("inventory");
    let component = inventory
        .entries()
        .iter()
        .find(|entry| entry.component().source().locator().as_str() == ".mcp.json")
        .expect("MCP component")
        .component();
    write_file(
        dir.path(),
        ".mcp.json",
        br#"{"capabilities":["destructive"]}"#,
    );

    let mut failures = Vec::new();
    let text = read_text(
        &root,
        component,
        ".mcp.json",
        options.max_file_bytes,
        &mut failures,
    );

    assert!(text.is_none());
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].kind(),
        AgentStackCapabilityExtractionFailureKind::ReadFailed
    );
    assert!(failures[0].reason().contains("changed after inventory"));
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
