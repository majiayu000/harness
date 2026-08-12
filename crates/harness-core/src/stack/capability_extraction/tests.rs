use super::*;
use std::{fs, path::Path};
use tempfile::TempDir;

use AgentStackCapability as Capability;
use AgentStackCapabilityExtractionConfidence as Confidence;
use AgentStackCapabilityExtractionConfidence::{High, Low, Medium};

type Row = (String, Capability, String, Confidence);
type ExpectedRow = (&'static str, Capability, &'static str, Confidence);
const POLICY: &str = "policy.explicit_capabilities";
const PREFIX: &str = "policy.prefix_rule";
const SCHEMA: &str = "mcp.input_schema";
const SERVER: &str = "mcp.server_declaration";
const META: &str = "hook.metadata_capabilities";
const STATIC: &str = "hook.static_command";

fn tmp() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write_control(root: &Path, rel: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

macro_rules! write_files {
    ($root:expr, $($path:expr => $contents:expr),+ $(,)?) => {
        $(write_control($root, $path, $contents);)+
    };
}

fn run(root: &Path) -> AgentStackCapabilityExtraction {
    extract_repository_capability_evidence(&AgentStackCapabilityExtractionOptions::new(
        root.to_path_buf(),
    ))
    .expect("extraction succeeds")
}

fn rows(extraction: &AgentStackCapabilityExtraction) -> Vec<Row> {
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

fn assert_rows(extraction: &AgentStackCapabilityExtraction, expected: &[ExpectedRow]) {
    let actual = rows(extraction);
    for expected in expected {
        assert!(
            actual.iter().any(|row| {
                row.0 == expected.0
                    && row.1 == expected.1
                    && row.2 == expected.2
                    && row.3 == expected.3
            }),
            "missing {expected:?} in {actual:#?}"
        );
    }
}

fn assert_empty(extraction: &AgentStackCapabilityExtraction) {
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
fn raised_file_limit_is_applied_to_inventory_and_extraction() {
    let dir = tmp();
    let padding = "x".repeat(1024 * 1024);
    write_control(
        dir.path(),
        ".mcp.json",
        format!(r#"{{"capabilities":["network"],"padding":"{padding}"}}"#),
    );
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf())
        .with_max_file_bytes(2 * 1024 * 1024)
        .expect("valid raised limit");
    let extraction = extract_repository_capability_evidence(&options)
        .expect("raised limit reaches inventory and extraction");
    assert_rows(
        &extraction,
        &[(PING, Capability::Network, "mcp.explicit_capabilities", High)],
    );
}

const PING: &str = ".mcp.json";

#[test]
fn changed_content_is_rejected_after_inventory() {
    let dir = tmp();
    write_control(dir.path(), PING, br#"{"capabilities":["network"]}"#);
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf());
    let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).expect("open root");
    let inventory = inventory_with_root(&root, &options.inventory_options).expect("inventory");
    let component = inventory
        .entries()
        .iter()
        .find(|entry| entry.component().source().locator().as_str() == PING)
        .expect("MCP component")
        .component();
    write_control(dir.path(), PING, br#"{"capabilities":["destructive"]}"#);
    let mut failures = Vec::new();
    assert!(read_text(
        &root,
        component,
        PING,
        options.max_file_bytes,
        &mut failures,
    )
    .is_none());
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].kind(),
        AgentStackCapabilityExtractionFailureKind::ReadFailed
    );
    assert!(failures[0].reason().contains("changed after inventory"));
}

#[test]
fn capability_extraction_reads_supported_declarations_and_static_commands() {
    let dir = tmp();
    write_files!(
        dir.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"policy.star\"]\n",
        ".vibeguard/policy.json5" => b"{ capabilities: ['network',], /* metadata */ }",
        "policy.star" => b"prefix_rule(pattern = [\"git\", \"push\"], decision = \"prompt\")\n",
        "rules/windows.md" => b"---\r\ncapabilities: [secret_read]\r\n---\r\n# Policy\r\n",
        PING => br#"{
  "tools":[{"inputSchema":{"properties":{
    "command":{"type":"string"},"output_path":{"type":"string"},
    "config":{"properties":{"api_key":{"type":"string"}}},
    "request":{"type":"array","items":{"properties":{"url":{"type":"string"}}}}
  }}}],
  "mcpServers":{
    "local":{"command":"node","args":["server.js"],"env":{"TOKEN":"secret"}},
    "remote":{"url":"https://mcp.invalid","headers":{"Authorization":"token"}}
  }
}"#,
        "requirements.toml" => b"[rules]\n[[rules.prefix_rules]]\npattern = [{ token = \"rm\" }]\ndecision = \"prompt\"\n",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\n# harness-reason:\n# harness-capabilities: shell\necho ready; curl https://example.invalid && rm -f output\n",
        ".harness/guards/pre-push.sh" => b"#!/bin/sh\n# harness-capabilities: network\ncurl https://example.invalid\n",
    );
    let extraction = run(dir.path());
    assert!(
        extraction.failures().is_empty(),
        "{:#?}",
        extraction.failures()
    );
    assert_rows(
        &extraction,
        &[
            (".vibeguard/policy.json5", Capability::Network, POLICY, High),
            ("policy.star", Capability::Destructive, PREFIX, Medium),
            ("rules/windows.md", Capability::SecretRead, POLICY, High),
            (PING, Capability::Shell, SCHEMA, Medium),
            (PING, Capability::Network, SCHEMA, Medium),
            (PING, Capability::SecretRead, SCHEMA, Medium),
            (PING, Capability::FileWrite, SCHEMA, Medium),
            (PING, Capability::Shell, SERVER, Medium),
            (PING, Capability::Network, SERVER, Medium),
            (PING, Capability::SecretRead, SERVER, Medium),
            ("requirements.toml", Capability::Destructive, PREFIX, Medium),
            (
                ".harness/guards/preflight.sh",
                Capability::Shell,
                META,
                High,
            ),
            (
                ".harness/guards/preflight.sh",
                Capability::Network,
                STATIC,
                Low,
            ),
            (
                ".harness/guards/preflight.sh",
                Capability::Destructive,
                STATIC,
                Low,
            ),
        ],
    );
    assert!(extraction
        .evidence()
        .iter()
        .all(|item| !item.reason().trim().is_empty()));
    let duplicate_rows = rows(&extraction)
        .into_iter()
        .filter(|row| row.0 == ".harness/guards/pre-push.sh" && row.1 == Capability::Network)
        .collect::<Vec<_>>();
    assert_eq!(duplicate_rows.len(), 1, "{duplicate_rows:#?}");
    assert_eq!(duplicate_rows[0].2, "hook.metadata_capabilities");
    assert_eq!(duplicate_rows[0].3, High);
}

#[test]
fn unsupported_and_documentation_sources_do_not_emit_evidence() {
    let dir = tmp();
    let outside = tmp();
    write_control(
        outside.path(),
        "outside.toml",
        b"capabilities = [\"destructive\"]\n",
    );
    write_files!(
        dir.path(),
        "harness.toml" => format!("[rules]\ndiscovery_paths = [\"{}\"]\n", outside.path().join("outside.toml").display()),
        ".harness/guards/binary.sh" => b"#!/bin/sh\n# harness-capabilities: shell\n\xff\xfe",
        ".harness/generated/policy.toml" => b"capabilities = [\"network\"]\n",
        "README.md" => b"capabilities = [\"destructive\"]\n",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\necho \"rm -rf /tmp/example\"\nprintf 'curl https://example.invalid'\n",
        "rules/security.md" => b"# Never run `rm -rf /`, `curl`, or `kubectl delete` from docs.\n",
    );
    assert_empty(&run(dir.path()));
}

#[test]
fn extractor_reports_parse_and_invalid_declaration_failures() {
    let dir = tmp();
    write_files!(
        dir.path(),
        PING => b"{ not valid json",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\n# harness-capabilities: rocket\n",
    );
    let extraction = run(dir.path());
    let has = |locator, kind, rule_id| {
        extraction.failures().iter().any(|failure| {
            failure.component().source().locator().as_str() == locator
                && failure.kind() == kind
                && failure.rule_id() == Some(rule_id)
        })
    };
    assert!(has(
        PING,
        AgentStackCapabilityExtractionFailureKind::ParseFailed,
        "typed.json_parse"
    ));
    assert!(has(
        ".harness/guards/preflight.sh",
        AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
        "hook.metadata_capabilities",
    ));
}
