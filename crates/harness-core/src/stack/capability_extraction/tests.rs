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

#[rustfmt::skip]
fn tmp() -> TempDir { tempfile::tempdir().expect("tempdir") }

#[rustfmt::skip]
fn write_control(root: &Path, rel: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).expect("mkdir"); }
    fs::write(path, contents).expect("write");
}

macro_rules! write_files {
    ($root:expr, $($path:expr => $contents:expr),+ $(,)?) => {
        $(write_control($root, $path, $contents);)+
    };
}

#[rustfmt::skip]
fn run(root: &Path) -> AgentStackCapabilityExtraction { extract_repository_capability_evidence(&AgentStackCapabilityExtractionOptions::new(root.to_path_buf())).expect("extraction succeeds") }

#[rustfmt::skip]
fn rows(extraction: &AgentStackCapabilityExtraction) -> Vec<Row<'_>> {
    extraction.evidence().iter().map(|item| (
        item.component().source().locator().as_str(), item.capability(), item.rule_id(), item.confidence()
    )).collect()
}
#[rustfmt::skip]
fn assert_rows(extraction: &AgentStackCapabilityExtraction, expected: &[ExpectedRow]) { assert_eq!(rows(extraction).as_slice(), expected); }

#[test]
#[rustfmt::skip]
fn raised_file_limit_is_applied_to_inventory_and_extraction() {
    let dir = tmp();
    let padding = "x".repeat(64 * 1024 * 1024);
    write_control(dir.path(), ".mcp.json", format!(r#"{{"capabilities":["network"],"padding":"{padding}"}}"#));
    let requested_limit = 65 * 1024 * 1024;
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf())
        .with_max_file_bytes(requested_limit)
        .expect("valid raised limit");
    assert_eq!(options.max_total_bytes, requested_limit);
    let extraction = extract_repository_capability_evidence(&options).expect("raised limit reaches inventory and extraction");
    assert_rows(&extraction, &[(PING, Capability::Network, "mcp.explicit_capabilities", High)]);
}

const PING: &str = ".mcp.json";

#[test]
#[rustfmt::skip]
fn changed_content_is_rejected_after_inventory() {
    let dir = tmp();
    let original = br#"{"capabilities":["network"]}"#;
    write_control(dir.path(), PING, original);
    let options = AgentStackCapabilityExtractionOptions::new(dir.path().to_path_buf());
    let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).expect("open root");
    let inventory = inventory_with_root(&root, &options.inventory_options).expect("inventory");
    let component = inventory.entries().iter().find(|entry| entry.component().source().locator().as_str() == PING).expect("MCP component").component();
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
        "policy.rules" => b"prefix_rule(pattern = [[\"curl\", \"rm\"]], decision = \"prompt\")\nprefix_rule(pattern = [\" python3 \"], decision = \"prompt\")\nprefix_rule([\"kubectl\", \"apply\"], \"prompt\")\nprefix_rule(pattern = [\"git\", \"push\"], decision = \"prompt\", match = [\"git push origin\"], not_match = [\"git status\"], justification = \"publishes changes\")\n[prefix_rule([\"curl\"])]\n",
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
            (PREFLIGHT, Capability::Destructive, STATIC, Low), (PREFLIGHT, Capability::FileWrite, STATIC, Low), (PREFLIGHT, Capability::Network, STATIC, Low), (PREFLIGHT, Capability::ProductionWrite, STATIC, Low), (PREFLIGHT, Capability::Shell, META, High),
            (PING, Capability::Destructive, SCHEMA, Medium), (PING, Capability::FileWrite, SCHEMA, Medium), (PING, Capability::Network, SCHEMA, Medium),
            (PING, Capability::Network, SERVER, Medium), (PING, Capability::ProductionWrite, SCHEMA, Medium), (PING, Capability::SecretRead, SCHEMA, Medium),
            (PING, Capability::SecretRead, SERVER, Medium), (PING, Capability::Shell, SCHEMA, Medium), (PING, Capability::Shell, SERVER, Medium),
            (".vibeguard/policy.json5", Capability::Network, POLICY, High),
            ("mcp.json", Capability::FileWrite, SCHEMA, Medium), ("mcp.json", Capability::Network, SCHEMA, Medium), ("mcp.json", Capability::SecretRead, SCHEMA, Medium),
            ("mcp.json", Capability::SecretRead, SERVER, Medium), ("mcp.json", Capability::Shell, SCHEMA, Medium),
            ("policy.rules", Capability::Destructive, PREFIX, Medium), ("policy.rules", Capability::FileWrite, PREFIX, Medium), ("policy.rules", Capability::Network, PREFIX, Medium),
            ("policy.rules", Capability::ProductionWrite, PREFIX, Medium), ("policy.rules", Capability::Shell, PREFIX, Medium),
            ("requirements.toml", Capability::Destructive, PREFIX, Medium), ("requirements.toml", Capability::FileWrite, PREFIX, Medium), ("requirements.toml", Capability::Network, PREFIX, Medium),
            ("rules/windows.md", Capability::SecretRead, POLICY, High),
        ],
    );
    assert!(extraction.evidence().iter().all(|item| !item.reason().trim().is_empty()));
    let schema_reasons = extraction.evidence().iter().filter(|item| item.component().source().locator().as_str() == "mcp.json" && item.rule_id() == SCHEMA).map(|item| item.reason()).collect::<Vec<_>>();
    assert!(schema_reasons.iter().any(|reason| reason.contains("APIToken"))); assert!(schema_reasons.iter().any(|reason| reason.contains("HTTPCommand")));
    assert!(schema_reasons.iter().any(|reason| reason.contains("URLValue"))); assert!(schema_reasons.iter().all(|reason| !reason.contains("tokenEndpoint")));
}

#[test]
#[rustfmt::skip]
fn shell_syntax_boundaries_preserve_real_commands_and_mutations() {
    let dir = tmp();
    let nested_wrappers = format!("{}curl https://example.invalid\n", "env ".repeat(32));
    write_files!(
        dir.path(),
        ".harness/guards/git-mutate.sh" => b"git -C . add file; git restore file\n",
        ".harness/guards/git-read.sh" => b"git -C . show push\n",
        ".harness/guards/hash.sh" => b"printf '%s' foo#bar; curl https://example.invalid\n",
        ".harness/guards/heredoc-hash.sh" => b"cat <<EOF#suffix\ncurl https://ignored.invalid\nEOF#suffix\ncat <<ESC\\ #suffix\ncurl https://also-ignored.invalid\nESC #suffix\n",
        ".harness/guards/fd.sh" => b"printf x >&2; printf x 2>&1; printf x 2>&-\n",
        ".harness/guards/redirect.sh" => b"printf '%s' value 2>>generated.conf\n",
        ".harness/guards/sudo.sh" => b"doas -a persist rm -rf output\nsudo -nEu root rm -rf output\n",
        ".harness/guards/substitution.sh" => b"echo \"$(curl https://example.invalid)\"\nresult=`rm -rf output`\nnested=`echo \\`curl https://example.invalid\\``\nprintf '%s' '$(kubectl apply)' '`wget ignored.invalid`'\n",
        ".harness/guards/heredoc-unquoted.sh" => b"cat <<EOF\n$(curl https://example.invalid)\nEOF\n",
        ".harness/guards/heredoc-tabs.sh" => b"cat <<-EOF\n\t`rm -rf output`\n\tEOF\n",
        ".harness/guards/heredoc-quoted.sh" => b"cat <<'EOF'\n$(curl https://ignored.invalid)\nEOF\n",
        ".harness/guards/heredoc-escaped.sh" => b"cat <<E\\OF\n`rm -rf ignored`\nEOF\n",
        ".harness/guards/heredoc-continuation.sh" => b"cat <<EOF\n$\\\n(curl https://example.invalid)\nEOF\n",
        ".harness/guards/nested-continuation.sh" => br#"echo "$(printf '%s' '"' ; cu\
rl https://example.invalid)"
"#,
        ".harness/guards/arithmetic-expansion.sh" => b"value=$(( 1\n << 2\n)); rm -rf output\n",
        ".harness/guards/arithmetic-multiline.sh" => b"(( x = 1\n << 2\n)); rm -rf output\n",
        ".harness/guards/arithmetic-shift.sh" => b"(( x = 1 << 2 )); rm -rf output\n",
        ".harness/guards/wrappers.sh" => b"env TOKEN=x curl https://example.invalid\ncommand rm -rf output\nexec kubectl apply\n",
        ".harness/guards/env-split.sh" => b"env -S 'rm -rf output'\n",
        ".harness/guards/env-split-trailing.sh" => b"env -S 'printf \"%s\\n\"' rm -rf output\n",
        ".harness/guards/env-split-escape.sh" => b"env -S \"rm\\_-rf\\_output\"\n",
        ".harness/guards/env-invalid.sh" => b"env -uS printf\nenv -PS printf\nenv -S \"'' rm -rf output\"\nenv -S 'c\\url https://example.invalid'\nenv -S '${}' rm -rf output\nenv -S '$NAME' rm -rf output\n",
        ".harness/guards/env-path.sh" => b"env -iP /usr/bin curl https://example.invalid\n",
        ".harness/guards/depth.sh" => nested_wrappers,
        ".harness/guards/shell-c.sh" => b"sh -c 'curl https://example.invalid; rm -rf output'\nsh -c 'echo \"$(rm -rf nested)\"'\n",
        ".harness/guards/gh-read.sh" => b"gh release list\ngh release view delete\ngh api -X GET search/issues -f q=repo:harness\ngh run download 7\ngh variable get NAME\ngh gist clone id\ngh codespace view\ngh repo clone owner/repo\ngh repo set-default --view\ngh pr checkout 42\n",
        ".harness/guards/gh-write.sh" => b"gh release create v1\ngh pr -R owner/repo merge 42\ngh label create urgent\ngh run cancel 7\ngh secret set TOKEN\ngh api -X DELETE repos/owner/repo/hooks/1\ngh api -XDELETE repos/owner/repo/hooks/1\ngh api -fbody=x repos/owner/repo/issues\n",
    );
    assert_rows(&run(dir.path()), &[
        (".harness/guards/arithmetic-expansion.sh", Capability::Destructive, STATIC, Low), (".harness/guards/arithmetic-expansion.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/arithmetic-multiline.sh", Capability::Destructive, STATIC, Low), (".harness/guards/arithmetic-multiline.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/arithmetic-shift.sh", Capability::Destructive, STATIC, Low), (".harness/guards/arithmetic-shift.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/depth.sh", Capability::Destructive, STATIC, Low), (".harness/guards/depth.sh", Capability::FileWrite, STATIC, Low), (".harness/guards/depth.sh", Capability::Network, STATIC, Low), (".harness/guards/depth.sh", Capability::Privileged, STATIC, Low), (".harness/guards/depth.sh", Capability::ProductionWrite, STATIC, Low), (".harness/guards/depth.sh", Capability::Shell, STATIC, Low),
        (".harness/guards/env-path.sh", Capability::Network, STATIC, Low),
        (".harness/guards/env-split-escape.sh", Capability::Destructive, STATIC, Low), (".harness/guards/env-split-escape.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/env-split.sh", Capability::Destructive, STATIC, Low), (".harness/guards/env-split.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/gh-read.sh", Capability::Network, STATIC, Low),
        (".harness/guards/gh-write.sh", Capability::Network, STATIC, Low), (".harness/guards/gh-write.sh", Capability::ProductionWrite, STATIC, Low),
        (".harness/guards/git-mutate.sh", Capability::Destructive, STATIC, Low), (".harness/guards/git-mutate.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/hash.sh", Capability::Network, STATIC, Low),
        (".harness/guards/heredoc-continuation.sh", Capability::Network, STATIC, Low),
        (".harness/guards/heredoc-tabs.sh", Capability::Destructive, STATIC, Low), (".harness/guards/heredoc-tabs.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/heredoc-unquoted.sh", Capability::Network, STATIC, Low),
        (".harness/guards/nested-continuation.sh", Capability::Network, STATIC, Low),
        (".harness/guards/redirect.sh", Capability::FileWrite, STATIC, Low),
        (".harness/guards/shell-c.sh", Capability::Destructive, STATIC, Low), (".harness/guards/shell-c.sh", Capability::FileWrite, STATIC, Low), (".harness/guards/shell-c.sh", Capability::Network, STATIC, Low), (".harness/guards/shell-c.sh", Capability::Shell, STATIC, Low),
        (".harness/guards/substitution.sh", Capability::Destructive, STATIC, Low), (".harness/guards/substitution.sh", Capability::FileWrite, STATIC, Low), (".harness/guards/substitution.sh", Capability::Network, STATIC, Low),
        (".harness/guards/sudo.sh", Capability::Destructive, STATIC, Low), (".harness/guards/sudo.sh", Capability::FileWrite, STATIC, Low), (".harness/guards/sudo.sh", Capability::Privileged, STATIC, Low),
        (".harness/guards/wrappers.sh", Capability::Destructive, STATIC, Low), (".harness/guards/wrappers.sh", Capability::FileWrite, STATIC, Low), (".harness/guards/wrappers.sh", Capability::Network, STATIC, Low), (".harness/guards/wrappers.sh", Capability::ProductionWrite, STATIC, Low),
    ]);
}

#[test]
#[rustfmt::skip]
fn ambiguous_multiline_substitutions_retain_capabilities_conservatively() {
    let dir = tmp();
    write_files!(
        dir.path(),
        ".harness/guards/comment-substitution.sh" => b"value=\"$( # ) is inside a comment\ncurl https://example.invalid\n)\"\n",
        ".harness/guards/heredoc-substitution.sh" => b"value=\"$(cat <<EOF\n)\nEOF\nrm -rf output\n)\"\n",
        ".harness/guards/backtick-comment.sh" => b"value=` # ` is inside a comment\ncurl https://example.invalid\n`\n",
    );
    let extraction = run(dir.path());
    for locator in [
        ".harness/guards/backtick-comment.sh",
        ".harness/guards/comment-substitution.sh",
        ".harness/guards/heredoc-substitution.sh",
    ] {
        for capability in [Capability::Destructive, Capability::FileWrite, Capability::Network, Capability::Privileged, Capability::ProductionWrite, Capability::Shell] {
            assert!(extraction.evidence().iter().any(|item| item.component().source().locator().as_str() == locator && item.capability() == capability));
        }
    }
}

#[test]
#[rustfmt::skip]
fn prefix_rule_trailing_alternatives_are_classified_without_unbounded_expansion() {
    let dir = tmp();
    write_files!(
        dir.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"policy.rules\"]\nrequirements_path = \"requirements.toml\"\n",
        "policy.rules" => b"prefix_rule(pattern = [\"git\", [\"status\", \"reset\"]], decision = \"prompt\")\n",
        "requirements.toml" => b"[[rules.prefix_rules]]\npattern = [{ token = \"git\" }, { any_of = [\"status\", \"reset\"] }]\ndecision = \"prompt\"\njustification = \"repository inspection\"\n",
    );
    assert_rows(&run(dir.path()), &[
        ("policy.rules", Capability::Destructive, PREFIX, Medium), ("policy.rules", Capability::FileWrite, PREFIX, Medium),
        ("requirements.toml", Capability::Destructive, PREFIX, Medium), ("requirements.toml", Capability::FileWrite, PREFIX, Medium),
    ]);

    let overflow = tmp();
    let alternatives = std::iter::repeat_n("\"status\"", 512).chain(std::iter::once("\"reset\"")).collect::<Vec<_>>().join(", ");
    write_files!(
        overflow.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"overflow.rules\"]\n",
        "overflow.rules" => format!("prefix_rule(pattern = [\"git\", [{alternatives}]], decision = \"prompt\")\n"),
    );
    let extraction = run(overflow.path());
    assert!(extraction.evidence().iter().any(|item| item.capability() == Capability::Destructive));

    let flat = tmp();
    let positions = std::iter::once("\"git\"").chain(std::iter::repeat_n("\"status\"", 256)).collect::<Vec<_>>().join(", ");
    write_files!(
        flat.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"flat.rules\"]\n",
        "flat.rules" => format!("prefix_rule(pattern = [{positions}], decision = \"prompt\")\n"),
    );
    let extraction = run(flat.path());
    assert!(extraction.evidence().is_empty());
    assert!(extraction.failures().iter().any(|failure| failure.kind() == FailureKind::LimitExceeded && failure.rule_id() == Some(PREFIX) && failure.reason().contains("position limit")));

    let bytes = tmp();
    let token = "x".repeat(64 * 1024 + 1);
    write_files!(
        bytes.path(),
        "harness.toml" => b"[rules]\nexec_policy_paths = [\"bytes.rules\"]\n",
        "bytes.rules" => format!("prefix_rule(pattern = [\"{token}\"], decision = \"prompt\")\n"),
    );
    let extraction = run(bytes.path());
    assert!(extraction.evidence().is_empty());
    assert!(extraction.failures().iter().any(|failure| failure.kind() == FailureKind::LimitExceeded && failure.rule_id() == Some(PREFIX) && failure.reason().contains("token byte limit")));
}

#[test]
#[rustfmt::skip]
fn typed_sources_honor_path_semantics_bom_docs_and_secret_references() {
    let dir = tmp();
    write_files!(
        dir.path(),
        "harness.toml" => b"[rules]\ndiscovery_paths = [\"custom.policy\", \"policies\"]\nbuiltin_path = \"builtins\"\nrequirements_path = \"policy.star\"\n",
        PING => br#"{"mcpServers":{"remote":{"env":{"CUSTOM_AUTH":"${GITHUB_TOKEN}"}}}}"#,
        ".vibeguard/example.json" => br#"{"examples":[{"harness_capabilities":["destructive"]}]}"#,
        "builtins/child.toml" => b"---\ncapabilities: [shell]\n---\n# Builtin\n",
        "custom.policy" => "\u{feff}---\ncapabilities: [secret_read]\n---\n# Custom\n",
        "policies/nested/policy.toml" => b"---\ncapabilities: [network]\n---\n# Nested\n",
        ".harness/rules/default.toml" => b"---\ncapabilities: [network]\n---\n# Default\n",
        "policy.star" => b"[rules]\n[[rules.prefix_rules]]\npattern = [{ token = \"curl\" }]\ndecision = \"prompt\"\n",
        "rules/bom.md" => "\u{feff}---\ncapabilities: [destructive]\n---\n# Policy\n",
    );
    assert_rows(&run(dir.path()), &[
        (".harness/rules/default.toml", Capability::Network, POLICY, High),
        (PING, Capability::SecretRead, SERVER, Medium), ("builtins/child.toml", Capability::Shell, POLICY, High),
        ("policies/nested/policy.toml", Capability::Network, POLICY, High),
        ("policy.star", Capability::Network, PREFIX, Medium),
    ]);
}

#[test]
#[rustfmt::skip]
fn unsupported_and_documentation_sources_do_not_emit_evidence() {
    let dir = tmp();
    let outside = tmp();
    write_control(outside.path(), "outside.toml", b"capabilities = [\"destructive\"]\n");
    write_files!(
        dir.path(),
        "harness.toml" => format!("[rules]\ndiscovery_paths = [\"{}\"]\n", outside.path().join("outside.toml").display()),
        ".gitignore" => b".mcp.json\nrules/\n",
        PING => br#"{"capabilities":["network"]}"#,
        ".harness/guards/binary.sh" => b"#!/bin/sh\n# harness-capabilities: shell\n\xff\xfe",
        ".vibeguard/generated/policy.toml" => b"capabilities = [\"network\"]\n",
        "README.md" => b"capabilities = [\"destructive\"]\n",
        "rules/.gitignore" => b"!reinclude.md\n",
        "rules/reinclude.md" => b"---\ncapabilities: [network]\n---\n",
        ".harness/guards/preflight.sh" => b"#!/bin/sh\necho \"rm -rf /tmp/example\"\nprintf 'curl https://example.invalid'\necho \"$((rm + 1))\"\necho foo\\; rm -rf output\necho \"start\n\\c\\u\\r\\l\" <<EOF\nrm -rf /tmp/example\n EOF\ncurl https://example.invalid\nEOF\n",
        "rules/security.md" => b"# Never run `rm -rf /`, `curl`, or `kubectl delete` from docs.\n",
    );
    let extraction = run(dir.path());
    assert!(extraction.evidence().is_empty());
    assert_eq!(extraction.failures().len(), 1);
    assert_eq!(extraction.failures()[0].kind(), FailureKind::ReadFailed);
}

#[test]
#[rustfmt::skip]
fn gitignore_semantics_cover_classes_spacing_negation_anchoring_and_double_star() {
    let dir = tmp();
    let policy = b"---\ncapabilities: [network]\n---\n";
    write_files!(
        dir.path(),
        ".gitignore" => b"rules/[0-9]*.md\nrules/**/generated-?.md\nrules/space\\ name.md\nrules/trailing.md   \n/rules/anchored.md\nrules/reinclude.md\n!rules/reinclude.md\n",
        "rules/1-number.md" => policy,
        "rules/a/b/generated-x.md" => policy,
        "rules/space name.md" => policy,
        "rules/trailing.md" => policy,
        "rules/anchored.md" => policy,
        "rules/nested/anchored.md" => policy,
        "rules/reinclude.md" => policy,
    );
    let extraction = run(dir.path());
    assert!(extraction.failures().is_empty(), "{:#?}", extraction.failures());
    assert_rows(&extraction, &[
        ("rules/nested/anchored.md", Capability::Network, POLICY, High),
        ("rules/reinclude.md", Capability::Network, POLICY, High),
    ]);
}

#[test]
#[rustfmt::skip]
fn nested_gitignore_matchers_stay_scoped_to_their_directory() {
    let dir = tmp();
    let policy = b"---\ncapabilities: [network]\n---\n";
    write_files!(
        dir.path(),
        "rules/a/.gitignore" => b"*.md\n",
        "rules/a/hidden.md" => policy,
        "rules/b/visible.md" => policy,
    );
    assert_rows(&run(dir.path()), &[("rules/b/visible.md", Capability::Network, POLICY, High)]);
}

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn symlinked_gitignore_files_are_not_followed() {
    use std::os::unix::fs::symlink;

    let dir = tmp();
    write_files!(
        dir.path(),
        ".ignore-source" => b"rules/*.md\n",
        "rules/visible.md" => b"---\ncapabilities: [network]\n---\n",
    );
    symlink(".ignore-source", dir.path().join(".gitignore")).expect("create gitignore symlink");
    assert_rows(&run(dir.path()), &[("rules/visible.md", Capability::Network, POLICY, High)]);
}

#[test]
#[rustfmt::skip]
fn hierarchical_gitignore_reads_share_byte_rule_and_pattern_budgets() {
    let dir = tmp();
    let mut locator = String::new();
    for index in 0..20 {
        if !locator.is_empty() { locator.push('/'); }
        locator.push_str(&format!("d{index}"));
        write_control(dir.path(), &format!("{locator}/.gitignore"), b"x\n");
    }
    locator.push_str("/rules/policy.md");
    let root = Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority()).expect("open root");
    let mut exclusions = RepositoryExclusions::default();
    let mut remaining_bytes = 10;
    let (kind, reason) = exclusions.excludes(&root, &locator, 1024, &mut remaining_bytes).expect_err("nested ignore files share the aggregate budget");
    assert_eq!(kind, FailureKind::LimitExceeded);
    assert!(reason.contains("total byte limit"));

    let rules = tmp();
    write_control(rules.path(), ".gitignore", "x\n".repeat(MAX_IGNORE_RULES + 1));
    let root = Dir::open_ambient_dir(rules.path(), cap_std::ambient_authority()).expect("open root");
    let mut exclusions = RepositoryExclusions::default();
    let mut remaining_bytes = DEFAULT_MAX_TOTAL_BYTES;
    let (kind, reason) = exclusions.excludes(&root, "rules/policy.md", DEFAULT_MAX_FILE_BYTES, &mut remaining_bytes).expect_err("ignore rule count is bounded");
    assert_eq!(kind, FailureKind::LimitExceeded);
    assert!(reason.contains("ignore rule limit"));

    let pattern = tmp();
    write_control(pattern.path(), ".gitignore", format!("{}\n", "x".repeat(MAX_IGNORE_PATTERN_BYTES + 1)));
    let root = Dir::open_ambient_dir(pattern.path(), cap_std::ambient_authority()).expect("open root");
    let mut exclusions = RepositoryExclusions::default();
    let mut remaining_bytes = DEFAULT_MAX_TOTAL_BYTES;
    let (kind, reason) = exclusions.excludes(&root, "rules/policy.md", DEFAULT_MAX_FILE_BYTES, &mut remaining_bytes).expect_err("ignore pattern length is bounded");
    assert_eq!(kind, FailureKind::LimitExceeded);
    assert!(reason.contains("ignore pattern"));
}

#[test]
#[rustfmt::skip]
fn extractor_reports_parse_and_invalid_declaration_failures() {
    let dir = tmp();
    let names = (0..typed::MAX_COMPONENT_FINDINGS).map(|index| format!("invalid{index}")).collect::<Vec<_>>().join(" ");
    let deeply_nested = format!("{}{{capabilities:['network']}}{}", "[".repeat(129), "]".repeat(129));
    let unicode_nested = format!("{{// comment\u{2028}{}0{} }}", "[".repeat(129), "]".repeat(129));
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
        "invalid.rules" => b"prefix_rule(pattern = [\"curl\"], decision = \"invalid\")\nprefix_rule(pattern = [\"curl\"], decision = \"prompt\", not_match = [\"curl\"])\n",
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

#[test]
fn unsupported_entry_does_not_consume_repository_finding_sentinel() {
    let dir = tmp();
    let full = (0..typed::MAX_COMPONENT_FINDINGS)
        .map(|index| format!("invalid{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let partial = (0..typed::MAX_COMPONENT_FINDINGS - 1)
        .map(|index| format!("invalid{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    for index in 0..3 {
        write_control(
            dir.path(),
            &format!(".harness/guards/limit-{index}.sh"),
            format!("#!/bin/sh\n# harness-capabilities: {full}\n"),
        );
    }
    write_control(
        dir.path(),
        ".harness/guards/limit-3.sh",
        format!("#!/bin/sh\n# harness-capabilities: {partial}\n"),
    );
    write_control(dir.path(), ".harness/guards/zzzz.txt", "ignored");
    let extraction = run(dir.path());
    assert_eq!(
        extraction.evidence().len() + extraction.failures().len(),
        MAX_REPOSITORY_FINDINGS - 1
    );
    assert!(!extraction
        .failures()
        .iter()
        .any(|failure| failure.rule_id() == Some(REPOSITORY_LIMIT_RULE_ID)));
}
