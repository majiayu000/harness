//! Parity guard: every entry point must build its agent registry from the one
//! shared builder.
//!
//! The drift this prevents was not hypothetical. Four hand-assembled copies had
//! diverged — the provider backpressure gate reached only `serve`,
//! `reasoning_budget` only `serve` and `exec`, adapters only `serve` and the
//! MCP server, and `anthropic-api` was missing from the MCP server entirely.
//! Asserting equality between two registries at runtime cannot catch that,
//! because the divergence lives in each call site's construction code. So this
//! asserts the structural property instead: no entry point constructs backends
//! itself.

use std::path::{Path, PathBuf};

/// Entry points that must own a registry, and the builder call each is
/// expected to make.
const ENTRY_POINTS: [(&str, &str); 4] = [
    ("src/commands/serve.rs", "registry_from_config"),
    ("src/commands/exec.rs", "registry_from_config"),
    ("src/gc.rs", "registry_from_config"),
    ("src/cmd/mcp_server.rs", "registry_from_config"),
];

/// Construction that belongs to the builder alone. A hit anywhere in
/// `harness-cli` production code means an entry point is assembling backends
/// by hand again, which is how the knobs drifted in the first place.
const FORBIDDEN_CONSTRUCTION: [&str; 4] = [
    "AgentRegistry::new(",
    "ClaudeCodeAgent::new(",
    "CodexAgent::from_config(",
    "ClaudeAdapter::new(",
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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
fn entry_points_build_their_registry_from_the_shared_builder() {
    for (relative, expected_call) in ENTRY_POINTS {
        let path = crate_dir().join(relative);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()));
        assert!(
            source.contains(expected_call),
            "{relative} must build its registry via `{expected_call}`"
        );
    }
}

#[test]
fn no_cli_source_assembles_agent_backends_by_hand() {
    let mut sources = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut sources);
    assert!(!sources.is_empty(), "lint found no sources to scan");

    let mut violations = Vec::new();
    for path in sources {
        let source = std::fs::read_to_string(&path).expect("readable source");
        for (index, line) in source.lines().enumerate() {
            // Test fixtures may build throwaway registries; only production
            // wiring has to funnel through the builder.
            if line.trim_start().starts_with("//") {
                continue;
            }
            for forbidden in FORBIDDEN_CONSTRUCTION {
                if line.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} — `{forbidden}` outside harness_agents::builder",
                        path.display(),
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
