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
    assert!(!read!().expect("first read is within budget").is_empty());
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
#[rustfmt::skip]
fn capability_extraction_reads_supported_declarations_and_static_commands() {
    let dir = tmp();
    let boundary_hook = format!("#!/bin/sh\nprintf '%s' \"long\nstring\"; \"/usr/bin/curl\" example.invalid\ncat <<< text\ncat <<\\A <<'B\\C'\ncurl body.invalid\nA\nrm body\nB\\C\ncat <<EOF \"multi\nline\"\nrm body\nEOF\ncat <<'终'\nrm body\n终\ncat <<''\nrm body\n\ncat <<E\\\nOF\nrm body\nEOF\ncat << -EOF\nrm body\n-EOF\n{}{}; wget https://example.invalid\n", "# filler\n".repeat(256), "x".repeat(4097));
    write_files!(
        dir.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"policy.rules\"]\n",
        ".vibeguard/policy.json5" => b"{ capabilities: ['network',], /* metadata */ }",
        "policy.rules" => b"prefix_rule(pattern = [[\"curl\", \"rm\"]], decision = \"prompt\")\nprefix_rule(pattern = [\" python3 \"], decision = \"prompt\")\nprefix_rule([\"kubectl\", \"apply\"], \"prompt\")\n[prefix_rule([\"curl\"])]\n",
        "rules/windows.md" => b"---\r\ncapabilities: [secret_read]\r\n---\r\n# Policy\r\n",
        PING => br#"{"tools":[{"inputSchema":{"properties":{
          "command":{},"output_path":{},"delete_flag":{},"TOKEN_FILE":{},"config":{"properties":{"api_key":{}}},
          "request":{"items":{"properties":{"url":{}}}},"definitions":{"job":{"properties":{"deploy":{}}}}
        }}}],"mcpServers":{"local":{"command":"node","env":{"TOKEN":"secret"}},
        "remote":{"url":"https://mcp.invalid","headers":{"Authorization":"token"}}}}"#,
        "mcp.json" => br#"{"examples":[{"harness_capabilities":["destructive"],"mcpServers":{"sample":{"command":"bash"}},"inputSchema":{"properties":{"command":{}}}}],"tools":[{"inputSchema":{"properties":{"profile":{},"tokenizer":{},"tokenEndpoint":{},"TOKENFile":{},"URLValue":{},"APIToken":{},"HTTPCommand":{}}}}],"mcpServers":{"benign":{"headers":{"User-Agent":"harness"},"env":{"NODE_ENV":"test"}},"secret":{"headers":{"X-Api-Key":"${API_KEY}"}},"empty":{"command":"","args":[],"url":" ","headers":{"Authorization":""},"env":{}}}}"#,
        "requirements.toml" => b"[rules]\n[[rules.prefix_rules]]\npattern = [{ any_of = [\"curl\", \"rm\"] }]\ndecision = \"prompt\"\n",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\n# harness-reason:\n# harness-capabilities: shell\necho ready; curl https://example.invalid && rm -f output\nif kubectl apply; then touch output; fi\n{ wget https://example.invalid; }\n",
        ".harness/guards/pre-push.sh" => b"#!/bin/sh\n# harness-capabilities: network\ncurl https://example.invalid\n",
        ".harness/guards/group.sh" => boundary_hook,
    );
    let extraction = run(dir.path());
    assert!(extraction.failures().is_empty(), "{:#?}", extraction.failures());
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
            (PING, Capability::Destructive, SCHEMA, Medium),
            (PING, Capability::FileWrite, SCHEMA, Medium),
            (PING, Capability::Network, SCHEMA, Medium),
            (PING, Capability::Network, SERVER, Medium),
            (PING, Capability::ProductionWrite, SCHEMA, Medium),
            (PING, Capability::SecretRead, SCHEMA, Medium),
            (PING, Capability::SecretRead, SERVER, Medium),
            (PING, Capability::Shell, SCHEMA, Medium),
            (PING, Capability::Shell, SERVER, Medium),
            (".vibeguard/policy.json5", Capability::Network, POLICY, High),
            ("mcp.json", Capability::FileWrite, SCHEMA, Medium),
            ("mcp.json", Capability::Network, SCHEMA, Medium),
            ("mcp.json", Capability::SecretRead, SCHEMA, Medium),
            ("mcp.json", Capability::SecretRead, SERVER, Medium),
            ("mcp.json", Capability::Shell, SCHEMA, Medium),
            ("policy.rules", Capability::Destructive, PREFIX, Medium),
            ("policy.rules", Capability::FileWrite, PREFIX, Medium),
            ("policy.rules", Capability::Network, PREFIX, Medium),
            ("policy.rules", Capability::ProductionWrite, PREFIX, Medium),
            ("policy.rules", Capability::Shell, PREFIX, Medium),
            ("requirements.toml", Capability::Destructive, PREFIX, Medium),
            ("requirements.toml", Capability::FileWrite, PREFIX, Medium),
            ("requirements.toml", Capability::Network, PREFIX, Medium),
            ("rules/windows.md", Capability::SecretRead, POLICY, High),
        ],
    );
    assert!(extraction.evidence().iter().all(|item| !item.reason().trim().is_empty()));
    let schema_reasons = extraction.evidence().iter().filter(|item| item.component().source().locator().as_str() == "mcp.json" && item.rule_id() == SCHEMA).map(|item| item.reason()).collect::<Vec<_>>();
    assert!(schema_reasons
        .iter()
        .any(|reason| reason.contains("APIToken")));
    assert!(schema_reasons
        .iter()
        .any(|reason| reason.contains("HTTPCommand")));
    assert!(schema_reasons.iter().any(|reason| reason.contains("URLValue")));
    assert!(schema_reasons.iter().all(|reason| !reason.contains("tokenEndpoint")));
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
        ".harness/guards/preflight.sh" => b"#!/bin/sh\necho \"rm -rf /tmp/example\"\nprintf 'curl https://example.invalid'\necho foo\\; rm -rf output\necho \"start\n\\c\\u\\r\\l\" <<EOF\nrm -rf /tmp/example\n EOF\ncurl https://example.invalid\nEOF\n",
        "rules/security.md" => b"# Never run `rm -rf /`, `curl`, or `kubectl delete` from docs.\n",
    );
    let extraction = run(dir.path());
    assert!(extraction.evidence().is_empty());
    assert_eq!(extraction.failures().len(), 1);
    assert_eq!(extraction.failures()[0].kind(), FailureKind::ReadFailed);
}

#[test]
#[rustfmt::skip]
fn extractor_reports_parse_and_invalid_declaration_failures() {
    let dir = tmp();
    let names = (0..typed::MAX_COMPONENT_FINDINGS).map(|index| format!("invalid{index}")).collect::<Vec<_>>().join(" ");
    let deeply_nested = format!("{}{{capabilities:['network']}}{}", "[".repeat(129), "]".repeat(129));
    let unicode_nested = format!(
        "{{// comment\u{2028}{}0{} }}",
        "[".repeat(129),
        "]".repeat(129)
    );
    write_files!(
        dir.path(),
        PING => b"{ not valid json",
        ".vibeguard/deep.json5" => deeply_nested,
        ".vibeguard/unicode.json5" => unicode_nested,
        ".vibeguard/comment.json5" => b"{capabilities:['network']} /*",
        ".vibeguard/invalid.json5" => b"{capabilities:['network', 7]}",
        ".vibeguard/prefix.json5" => b"{rules:{prefix_rules:'curl'}}",
        ".vibeguard/decision.json5" => b"{rules:{prefix_rules:[{pattern:'curl'}]}}",
        "harness.toml" => b"[rules]\ndiscovery_paths = [\"harness.toml\"]\nexec_policy_paths = [\"invalid.rules\"]\nrequirements_path = \"./reqs.req\"\n",
        "invalid.rules" => b"prefix_rule(pattern = [\"curl\"], decision = \"invalid\")\n",
        "reqs.req" => b"foo = 1\n",
        "requirements.toml" => b"[[rules.prefix_rules]]\npattern = [\"curl\"]\ndecision = \"prompt\"\n[[rules.prefix_rules]]\npattern = [{ token = \"rm\" }]\ndecision = \"allow\"\n[[rules.prefix_rules]]\npattern = [{ token = \"curl\" }]\ndecision = \"prompt\"\njustification = \"\"\n",
        "rules/unclosed.md" => b"---\ncapabilities: [network]\n",
        "rules/invalid-open.md" => b"---oops\ncapabilities: [network]\n---\n",
        ".harness/guards/preflight.sh" => format!("#!/bin/sh\n# harness-capabilities: {names}\ncurl https://example.invalid\n"),
        ".harness/guards/pre-push.sh" => format!("#!/bin/sh\n# harness-capabilities: {}\ncurl example.invalid; rm output\n", names.split(' ').take(typed::MAX_COMPONENT_FINDINGS - 2).collect::<Vec<_>>().join(" ")),
        ".harness/guards/empty.sh" => b"#!/bin/sh\n# harness-capabilities:\n",
    );
    let extraction = run(dir.path());
    let has = |locator, kind, rule_id| extraction.failures().iter().any(|failure| failure.component().source().locator().as_str() == locator && failure.kind() == kind && failure.rule_id() == Some(rule_id));
    use FailureKind::{InvalidDeclaration, ParseFailed};
    for (locator, kind, rule) in [
        (PING, ParseFailed, "typed.json_parse"),
        (".vibeguard/deep.json5", ParseFailed, "typed.json5_parse"),
        (".vibeguard/unicode.json5", ParseFailed, "typed.json5_parse"),
        (".vibeguard/comment.json5", ParseFailed, "typed.json5_parse"),
        (".vibeguard/invalid.json5", InvalidDeclaration, POLICY),
        (".vibeguard/prefix.json5", InvalidDeclaration, PREFIX),
        (".vibeguard/decision.json5", InvalidDeclaration, PREFIX),
        ("invalid.rules", InvalidDeclaration, PREFIX),
        ("reqs.req", InvalidDeclaration, PREFIX),
        ("requirements.toml", InvalidDeclaration, PREFIX),
        ("rules/unclosed.md", ParseFailed, "typed.front_matter_parse"),
        ("rules/invalid-open.md", ParseFailed, "typed.front_matter_parse"),
        (PREFLIGHT, InvalidDeclaration, "hook.metadata_capabilities"),
        (PREFLIGHT, FailureKind::LimitExceeded, typed::LIMIT_RULE_ID),
        (PRE_PUSH, FailureKind::LimitExceeded, typed::LIMIT_RULE_ID),
        (".harness/guards/empty.sh", InvalidDeclaration, "hook.metadata_capabilities"),
    ] {
        assert!(has(locator, kind, rule));
    }
    for locator in [PREFLIGHT, PRE_PUSH] {
        let failures = extraction.failures().iter().filter(|failure| failure.component().source().locator().as_str() == locator).count();
        let evidence = extraction.evidence().iter().filter(|item| item.component().source().locator().as_str() == locator).count();
        assert_eq!(failures + evidence, typed::MAX_COMPONENT_FINDINGS);
    }
}

#[test]
#[rustfmt::skip]
fn repository_finding_limit_is_fail_visible() {
    let dir = tmp();
    let names = (0..typed::MAX_COMPONENT_FINDINGS).map(|index| format!("invalid{index}")).collect::<Vec<_>>().join(" ");
    for index in 0..5 {
        write_control(dir.path(), &format!(".harness/guards/limit-{index}.sh"), format!("#!/bin/sh\n# harness-capabilities: {names}\n"));
    }
    let extraction = run(dir.path());
    assert_eq!(extraction.evidence().len() + extraction.failures().len(), MAX_REPOSITORY_FINDINGS);
    assert!(extraction.failures().iter().any(|failure| failure.rule_id() == Some(REPOSITORY_LIMIT_RULE_ID)));
}
