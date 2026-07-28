//! Parity guard: every CLI entry point must build agents from the shared
//! builders.
//!
//! The drift this prevents was not hypothetical. Four hand-assembled copies had
//! diverged — the provider backpressure gate reached only `serve`,
//! `reasoning_budget` only `serve` and `exec`, adapters only `serve` and the
//! MCP server, and `anthropic-api` was missing from the MCP server entirely.
//! Asserting equality between two registries at runtime cannot catch that,
//! because the divergence lives in each call site's construction code. So this
//! asserts the structural property instead: no entry point constructs backends
//! itself, apart from the separately configured read-only PR review provider.

use std::path::{Path, PathBuf};

/// CLI paths that construct configured agents, and the exact shared-builder
/// invocation each must make once.
const REQUIRED_BUILDER_CALLS: [(&str, &str); 5] = [
    (
        "src/commands/serve.rs",
        "harness_agents::builder::registry_from_config(",
    ),
    (
        "src/commands/exec.rs",
        "harness_agents::builder::registry_from_config(",
    ),
    (
        "src/gc.rs",
        "harness_agents::builder::registry_from_config(",
    ),
    (
        "src/cmd/mcp_server.rs",
        "harness_agents::builder::registry_from_config(",
    ),
    (
        "src/cmd/pr.rs",
        "harness_agents::builder::claude_agent_from_config(",
    ),
];

/// Types whose construction belongs to `harness_agents::builder`. Matching
/// the type qualifier catches alternate and future associated constructors
/// instead of maintaining a method-by-method denylist.
const FORBIDDEN_TYPE_QUALIFIERS: [&str; 7] = [
    "AgentRegistry::",
    "ClaudeCodeAgent::",
    "CodexAgent::",
    "ClaudeAdapter::",
    "CodexAdapter::",
    "AnthropicApiAgent::",
    "ProviderBackpressureGate::",
];

/// The PR review provider has its own config shape and intentionally creates
/// one read-only Codex agent outside the normal agent registry.
const ALLOWED_DIRECT_CONSTRUCTION: (&str, &str, usize) = ("src/cmd/pr.rs", "CodexAgent::new(", 1);

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn code_occurrences(source: &str, needle: &str) -> usize {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.matches(needle).count())
        .sum()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("readable source directory");
    for entry in entries {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn required_cli_paths_call_the_shared_builder() {
    for (relative, expected_call) in REQUIRED_BUILDER_CALLS {
        let path = crate_dir().join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()));
        assert_eq!(
            code_occurrences(&source, expected_call),
            1,
            "{relative} must invoke `{expected_call}` exactly once"
        );
    }
}

#[test]
fn no_cli_source_assembles_agent_backends_by_hand() {
    let mut sources = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut sources);
    assert!(!sources.is_empty(), "lint found no sources to scan");

    let (allowed_path, allowed_call, expected_count) = ALLOWED_DIRECT_CONSTRUCTION;
    let allowed_source = std::fs::read_to_string(crate_dir().join(allowed_path))
        .unwrap_or_else(|error| panic!("{allowed_path} should be readable: {error}"));
    assert_eq!(
        code_occurrences(&allowed_source, allowed_call),
        expected_count,
        "{allowed_path} must contain exactly {expected_count} intentional `{allowed_call}` call"
    );

    let mut violations = Vec::new();
    for path in sources {
        let source = std::fs::read_to_string(&path).expect("readable source");
        let relative = path
            .strip_prefix(crate_dir())
            .expect("source should be inside harness-cli");
        for (index, line) in source.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let code = if relative == Path::new(allowed_path) {
                line.replace(allowed_call, "")
            } else {
                line.to_string()
            };
            for forbidden in FORBIDDEN_TYPE_QUALIFIERS {
                if code.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} — `{forbidden}` construction outside harness_agents::builder",
                        relative.display(),
                        index + 1
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "agent backends must be constructed by `harness_agents::builder`, \
         so every entry point gets the same configuration:\n{}",
        violations.join("\n")
    );
}
