use super::*;
use std::{fs, path::Path};
use tempfile::TempDir;

use AgentStackCapability as Capability;
use AgentStackCapabilityExtractionConfidence as Confidence;
use AgentStackCapabilityExtractionConfidence::{High, Low, Medium};
use AgentStackCapabilityExtractionFailureKind as FailureKind;

type Row<'a> = (&'a str, Capability, &'a str, Confidence);
type ExpectedRow = (&'static str, Capability, &'static str, Confidence);
const POLICY: &str = "policy.explicit_capabilities";
const PREFIX: &str = "policy.prefix_rule";
const SCHEMA: &str = "mcp.input_schema";
const SERVER: &str = "mcp.server_declaration";
const META: &str = "hook.metadata_capabilities";
const STATIC: &str = "hook.static_command";
const PREFLIGHT: &str = ".harness/guards/preflight.sh";
const PRE_PUSH: &str = ".harness/guards/pre-push.sh";

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

#[rustfmt::skip]
fn rows(extraction: &AgentStackCapabilityExtraction) -> Vec<Row<'_>> {
    extraction.evidence().iter().map(|item| (
        item.component().source().locator().as_str(), item.capability(), item.rule_id(), item.confidence()
    )).collect()
}
fn assert_rows(extraction: &AgentStackCapabilityExtraction, expected: &[ExpectedRow]) {
    assert_eq!(rows(extraction).as_slice(), expected);
}

#[test]
fn raised_file_limit_is_applied_to_inventory_and_extraction() {
    let dir = tmp();
    let padding = "x".repeat(64 * 1024 * 1024);
    write_control(
        dir.path(),
        ".mcp.json",
        format!(r#"{{"capabilities":["network"],"padding":"{padding}"}}"#),
    );
    let requested_limit = 65 * 1024 * 1024;
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf())
        .with_max_file_bytes(requested_limit)
        .expect("valid raised limit");
    assert_eq!(options.max_total_bytes, requested_limit);
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
    let original = br#"{"capabilities":["network"]}"#;
    write_control(dir.path(), PING, original);
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf());
    let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).expect("open root");
    let inventory = inventory_with_root(&root, &options.inventory_options).expect("inventory");
    let component = inventory
        .entries()
        .iter()
        .find(|entry| entry.component().source().locator().as_str() == PING)
        .expect("MCP component")
        .component();
    let mut remaining_bytes = original.len() as u64;
    #[rustfmt::skip]
    macro_rules! read { () => { read_text(&root, component, PING, options.max_file_bytes, &mut remaining_bytes) }; }
    assert!(read!().expect("first read is within budget").is_some());
    let (kind, reason) = read!().expect_err("cumulative limit applies");
    assert_eq!(kind, FailureKind::LimitExceeded);
    assert!(reason.contains("total byte limit"));
    write_control(dir.path(), PING, br#"{"capabilities":["destructive"]}"#);
    remaining_bytes = options.max_total_bytes;
    let (kind, reason) = read!().expect_err("changed content is rejected");
    assert_eq!(kind, FailureKind::ReadFailed);
    assert!(reason.contains("changed after inventory"));
}

#[test]
fn capability_extraction_reads_supported_declarations_and_static_commands() {
    let dir = tmp();
    write_files!(
        dir.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"policy.star\"]\n",
        ".vibeguard/policy.json5" => b"{ capabilities: ['network',], /* metadata */ }",
        "policy.star" => b"prefix_rule(pattern = [[\"curl\", \"rm\"]], decision = \"prompt\")\nprefix_rule(pattern = [\" python3 \"], decision = \"prompt\")\nprefix_rule([\"kubectl\", \"apply\"], \"prompt\")\n",
        "rules/windows.md" => b"---\r\ncapabilities: [secret_read]\r\n---\r\n# Policy\r\n",
        PING => br#"{"tools":[{"inputSchema":{"properties":{
          "command":{},"output_path":{},"config":{"properties":{"api_key":{}}},
          "request":{"items":{"properties":{"url":{}}}},"definitions":{"job":{"properties":{"deploy":{}}}}
        }}}],"mcpServers":{"local":{"command":"node","env":{"TOKEN":"secret"}},
        "remote":{"url":"https://mcp.invalid","headers":{"Authorization":"token"}}}}"#,
        "mcp.json" => br#"{"examples":[{"capabilities":["destructive"]}],"tools":[{"inputSchema":{"properties":{"profile":{},"tokenizer":{}}}}],"mcpServers":{"benign":{"headers":{"User-Agent":"harness"},"env":{"NODE_ENV":"test"}},"secret":{"headers":{"X-Api-Key":"${API_KEY}"}},"empty":{"command":"","args":[],"url":" ","headers":{"Authorization":""},"env":{}}}}"#,
        "requirements.toml" => b"[rules]\n[[rules.prefix_rules]]\npattern = [{ any_of = [\"curl\", \"rm\"] }]\ndecision = \"prompt\"\n",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\n# harness-reason:\n# harness-capabilities: shell\necho ready; curl https://example.invalid && rm -f output\nif kubectl apply; then touch output; fi\n{ wget https://example.invalid; }\n",
        ".harness/guards/pre-push.sh" => b"#!/bin/sh\n# harness-capabilities: network\ncurl https://example.invalid\n",
        ".harness/guards/group.sh" => b"#!/bin/sh\n{ wget https://example.invalid; }\n",
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
            (".harness/guards/group.sh", Capability::Network, STATIC, Low),
            (PRE_PUSH, Capability::Network, META, High),
            (PREFLIGHT, Capability::Destructive, STATIC, Low),
            (PREFLIGHT, Capability::FileWrite, STATIC, Low),
            (PREFLIGHT, Capability::Network, STATIC, Low),
            (PREFLIGHT, Capability::ProductionWrite, STATIC, Low),
            (PREFLIGHT, Capability::Shell, META, High),
            (PING, Capability::FileWrite, SCHEMA, Medium),
            (PING, Capability::Network, SCHEMA, Medium),
            (PING, Capability::Network, SERVER, Medium),
            (PING, Capability::ProductionWrite, SCHEMA, Medium),
            (PING, Capability::SecretRead, SCHEMA, Medium),
            (PING, Capability::SecretRead, SERVER, Medium),
            (PING, Capability::Shell, SCHEMA, Medium),
            (PING, Capability::Shell, SERVER, Medium),
            (".vibeguard/policy.json5", Capability::Network, POLICY, High),
            ("mcp.json", Capability::SecretRead, SERVER, Medium),
            ("policy.star", Capability::Destructive, PREFIX, Medium),
            ("policy.star", Capability::FileWrite, PREFIX, Medium),
            ("policy.star", Capability::Network, PREFIX, Medium),
            ("policy.star", Capability::ProductionWrite, PREFIX, Medium),
            ("policy.star", Capability::Shell, PREFIX, Medium),
            ("requirements.toml", Capability::Destructive, PREFIX, Medium),
            ("requirements.toml", Capability::FileWrite, PREFIX, Medium),
            ("requirements.toml", Capability::Network, PREFIX, Medium),
            ("rules/windows.md", Capability::SecretRead, POLICY, High),
        ],
    );
    assert!(extraction
        .evidence()
        .iter()
        .all(|item| !item.reason().trim().is_empty()));
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
        ".harness/guards/preflight.sh" => b"#!/bin/sh\necho \"rm -rf /tmp/example\"\nprintf 'curl https://example.invalid'\necho foo\\; rm -rf output\n",
        "rules/security.md" => b"# Never run `rm -rf /`, `curl`, or `kubectl delete` from docs.\n",
    );
    let extraction = run(dir.path());
    assert!(extraction.evidence().is_empty() && extraction.failures().is_empty());
}

#[test]
fn extractor_reports_parse_and_invalid_declaration_failures() {
    let dir = tmp();
    let names = (0..typed::MAX_COMPONENT_FINDINGS)
        .map(|index| format!("invalid{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let deeply_nested = format!(
        "{}{{capabilities:['network']}}{}",
        "[".repeat(129),
        "]".repeat(129)
    );
    write_files!(
        dir.path(),
        PING => b"{ not valid json",
        ".vibeguard/deep.json5" => deeply_nested,
        ".vibeguard/comment.json5" => b"{capabilities:['network']} /*",
        ".vibeguard/invalid.json5" => b"{capabilities:['network', 7]}",
        ".harness/guards/preflight.sh" => format!("#!/bin/sh\n# harness-capabilities: {names}\ncurl https://example.invalid\n"),
        ".harness/guards/pre-push.sh" => format!("#!/bin/sh\n# harness-capabilities: {}\ncurl example.invalid; rm output\n", names.split(' ').take(typed::MAX_COMPONENT_FINDINGS - 2).collect::<Vec<_>>().join(" ")),
        ".harness/guards/empty.sh" => b"#!/bin/sh\n# harness-capabilities:\n",
    );
    let extraction = run(dir.path());
    let has = |locator, kind, rule_id| {
        extraction.failures().iter().any(|failure| {
            failure.component().source().locator().as_str() == locator
                && failure.kind() == kind
                && failure.rule_id() == Some(rule_id)
        })
    };
    use FailureKind::{InvalidDeclaration, ParseFailed};
    for (locator, kind, rule) in [
        (PING, ParseFailed, "typed.json_parse"),
        (".vibeguard/deep.json5", ParseFailed, "typed.json5_parse"),
        (".vibeguard/comment.json5", ParseFailed, "typed.json5_parse"),
        (".vibeguard/invalid.json5", InvalidDeclaration, POLICY),
        (PREFLIGHT, InvalidDeclaration, "hook.metadata_capabilities"),
        (PREFLIGHT, FailureKind::LimitExceeded, typed::LIMIT_RULE_ID),
        (PRE_PUSH, FailureKind::LimitExceeded, typed::LIMIT_RULE_ID),
        (
            ".harness/guards/empty.sh",
            InvalidDeclaration,
            "hook.metadata_capabilities",
        ),
    ] {
        assert!(has(locator, kind, rule));
    }
    for locator in [PREFLIGHT, PRE_PUSH] {
        let failures = extraction
            .failures()
            .iter()
            .filter(|failure| failure.component().source().locator().as_str() == locator)
            .count();
        let evidence = extraction
            .evidence()
            .iter()
            .filter(|item| item.component().source().locator().as_str() == locator)
            .count();
        assert_eq!(failures + evidence, typed::MAX_COMPONENT_FINDINGS);
    }
}
