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

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use syn::{
    visit::{self, Visit},
    Expr, ExprCall, ExprPath, ExprStruct, ItemType, ItemUse, Path as SynPath, Type, UseTree,
};

/// CLI paths that construct configured agents, and the exact shared-builder
/// invocation each must make once.
const REQUIRED_BUILDER_CALLS: [(&str, &str); 5] = [
    (
        "src/commands/serve.rs",
        "harness_agents::builder::registry_from_config",
    ),
    (
        "src/commands/exec.rs",
        "harness_agents::builder::registry_from_config",
    ),
    ("src/gc.rs", "harness_agents::builder::registry_from_config"),
    (
        "src/cmd/mcp_server.rs",
        "harness_agents::builder::registry_from_config",
    ),
    (
        "src/cmd/pr.rs",
        "harness_agents::builder::claude_agent_from_config",
    ),
];

/// Types whose construction belongs to `harness_agents::builder`. Matching
/// the type qualifier catches alternate and future associated constructors
/// instead of maintaining a method-by-method denylist.
const FORBIDDEN_TYPES: [&str; 7] = [
    "AgentRegistry",
    "ClaudeCodeAgent",
    "CodexAgent",
    "ClaudeAdapter",
    "CodexAdapter",
    "AnthropicApiAgent",
    "ProviderBackpressureGate",
];

/// The PR review provider has its own config shape and intentionally creates
/// one read-only Codex agent outside the normal agent registry.
const ALLOWED_DIRECT_CONSTRUCTION: (&str, &str, &str, usize) =
    ("src/cmd/pr.rs", "CodexAgent", "new", 1);

#[derive(Debug, PartialEq, Eq)]
struct DirectConstruction {
    type_name: String,
    associated_item: Option<String>,
    syntax: String,
    is_call: bool,
}

#[derive(Default)]
struct SourceAnalysis {
    called_paths: Vec<Vec<String>>,
    direct_constructions: Vec<DirectConstruction>,
}

impl SourceAnalysis {
    fn call_count(&self, expected_path: &str) -> usize {
        let expected = expected_path.split("::").collect::<Vec<_>>();
        self.called_paths
            .iter()
            .filter(|path| path_matches(path, &expected))
            .count()
    }
}

#[derive(Default)]
struct Aliases {
    paths: HashMap<String, Vec<String>>,
}

impl Aliases {
    fn resolve_path(&self, path: &SynPath) -> Vec<String> {
        self.resolve_segments(path_segments(path))
    }

    fn resolve_segments(&self, mut segments: Vec<String>) -> Vec<String> {
        let mut visited = HashSet::new();
        while let Some(first) = segments.first().cloned() {
            let Some(target) = self.paths.get(&first) else {
                break;
            };
            if !visited.insert(first) {
                break;
            }

            let mut expanded = target.clone();
            expanded.extend(segments.into_iter().skip(1));
            segments = expanded;
        }
        segments
    }

    fn forbidden_type_at_path_end(&self, segments: &[String]) -> Option<String> {
        let resolved = self.resolve_segments(segments.to_vec());
        resolved
            .last()
            .filter(|name| is_forbidden_type(name))
            .cloned()
            .or_else(|| {
                segments.last().and_then(|name| {
                    let resolved_name = self.resolve_segments(vec![name.clone()]);
                    resolved_name
                        .last()
                        .filter(|resolved| is_forbidden_type(resolved))
                        .cloned()
                })
            })
    }

    fn forbidden_type_before_last(&self, segments: &[String]) -> Option<String> {
        let resolved = self.resolve_segments(segments.to_vec());
        resolved[..resolved.len().saturating_sub(1)]
            .iter()
            .rev()
            .find(|name| is_forbidden_type(name))
            .cloned()
            .or_else(|| {
                segments
                    .get(..segments.len().saturating_sub(1))
                    .and_then(|qualifiers| {
                        qualifiers.iter().rev().find_map(|name| {
                            self.forbidden_type_at_path_end(std::slice::from_ref(name))
                        })
                    })
            })
    }
}

#[derive(Default)]
struct AliasCollector {
    aliases: Aliases,
}

impl<'ast> Visit<'ast> for AliasCollector {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        collect_use_aliases(&item.tree, &mut Vec::new(), &mut self.aliases.paths);
        visit::visit_item_use(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast ItemType) {
        if let Type::Path(type_path) = item.ty.as_ref() {
            if type_path.qself.is_none() {
                self.aliases
                    .paths
                    .insert(item.ident.to_string(), path_segments(&type_path.path));
            }
        }
        visit::visit_item_type(self, item);
    }
}

struct SourceScanner<'a> {
    aliases: &'a Aliases,
    analysis: SourceAnalysis,
}

impl SourceScanner<'_> {
    fn record_expr_path(&mut self, expression: &ExprPath, is_call: bool) {
        if let Some(qself) = &expression.qself {
            if let Type::Path(type_path) = qself.ty.as_ref() {
                if let Some(type_name) = self
                    .aliases
                    .forbidden_type_at_path_end(&path_segments(&type_path.path))
                {
                    self.analysis.direct_constructions.push(DirectConstruction {
                        type_name,
                        associated_item: expression
                            .path
                            .segments
                            .last()
                            .map(|segment| segment.ident.to_string()),
                        syntax: format!(
                            "<{}>::{}",
                            path_segments(&type_path.path).join("::"),
                            path_segments(&expression.path).join("::")
                        ),
                        is_call,
                    });
                    return;
                }
            }
        }

        let original = path_segments(&expression.path);
        let resolved = self.aliases.resolve_segments(original.clone());
        let associated_item = resolved.last().cloned();
        if let Some(type_name) = self.aliases.forbidden_type_before_last(&original) {
            self.analysis.direct_constructions.push(DirectConstruction {
                type_name,
                associated_item,
                syntax: original.join("::"),
                is_call,
            });
        } else if let Some(type_name) = self.aliases.forbidden_type_at_path_end(&original) {
            self.analysis.direct_constructions.push(DirectConstruction {
                type_name,
                associated_item: None,
                syntax: original.join("::"),
                is_call,
            });
        }
    }
}

impl<'ast> Visit<'ast> for SourceScanner<'_> {
    fn visit_expr_call(&mut self, expression: &'ast ExprCall) {
        if let Expr::Path(function) = expression.func.as_ref() {
            if function.qself.is_none() {
                self.analysis
                    .called_paths
                    .push(self.aliases.resolve_path(&function.path));
            }
            self.record_expr_path(function, true);
            if let Some(qself) = &function.qself {
                visit::visit_qself(self, qself);
            }
            visit::visit_path(self, &function.path);
            for argument in &expression.args {
                self.visit_expr(argument);
            }
            return;
        }
        visit::visit_expr_call(self, expression);
    }

    fn visit_expr_path(&mut self, expression: &'ast ExprPath) {
        self.record_expr_path(expression, false);
        visit::visit_expr_path(self, expression);
    }

    fn visit_expr_struct(&mut self, expression: &'ast ExprStruct) {
        let original = path_segments(&expression.path);
        if let Some(type_name) = self.aliases.forbidden_type_at_path_end(&original) {
            self.analysis.direct_constructions.push(DirectConstruction {
                type_name,
                associated_item: None,
                syntax: format!("{} {{ .. }}", original.join("::")),
                is_call: false,
            });
        }
        visit::visit_expr_struct(self, expression);
    }
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn is_forbidden_type(name: &str) -> bool {
    FORBIDDEN_TYPES.contains(&name)
}

fn path_segments(path: &SynPath) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn path_matches(path: &[String], expected: &[&str]) -> bool {
    path.iter().map(String::as_str).eq(expected.iter().copied())
}

fn collect_use_aliases(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut HashMap<String, Vec<String>>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_aliases(&path.tree, prefix, aliases);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let imported = name.ident.to_string();
            if imported == "self" {
                if let Some(local_name) = prefix.last() {
                    aliases.insert(local_name.clone(), prefix.clone());
                }
            } else {
                let mut target = prefix.clone();
                target.push(imported.clone());
                aliases.insert(imported, target);
            }
        }
        UseTree::Rename(rename) => {
            let imported = rename.ident.to_string();
            let mut target = prefix.clone();
            if imported != "self" {
                target.push(imported);
            }
            aliases.insert(rename.rename.to_string(), target);
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_aliases(item, prefix, aliases);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn analyze_source(source: &str) -> syn::Result<SourceAnalysis> {
    let file = syn::parse_file(source)?;
    let mut collector = AliasCollector::default();
    collector.visit_file(&file);

    let mut scanner = SourceScanner {
        aliases: &collector.aliases,
        analysis: SourceAnalysis::default(),
    };
    scanner.visit_file(&file);
    Ok(scanner.analysis)
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
        let analysis = analyze_source(&source)
            .unwrap_or_else(|error| panic!("{} should parse as Rust: {error}", path.display()));
        assert_eq!(
            analysis.call_count(expected_call),
            1,
            "{relative} must invoke `{expected_call}` exactly once"
        );
    }
}

#[test]
fn no_cli_source_assembles_agent_backends_by_hand() {
    let mut sources = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut sources);
    sources.sort();
    assert!(!sources.is_empty(), "lint found no sources to scan");

    let (allowed_path, allowed_type, allowed_item, expected_count) = ALLOWED_DIRECT_CONSTRUCTION;
    let allowed_source = std::fs::read_to_string(crate_dir().join(allowed_path))
        .unwrap_or_else(|error| panic!("{allowed_path} should be readable: {error}"));
    let allowed_analysis = analyze_source(&allowed_source)
        .unwrap_or_else(|error| panic!("{allowed_path} should parse as Rust: {error}"));
    assert_eq!(
        allowed_analysis
            .direct_constructions
            .iter()
            .filter(|construction| {
                construction.type_name == allowed_type
                    && construction.associated_item.as_deref() == Some(allowed_item)
                    && construction.is_call
            })
            .count(),
        expected_count,
        "{allowed_path} must contain exactly {expected_count} intentional \
         `{allowed_type}::{allowed_item}` call"
    );

    let mut violations = Vec::new();
    for path in sources {
        let source = std::fs::read_to_string(&path).expect("readable source");
        let relative = path
            .strip_prefix(crate_dir())
            .expect("source should be inside harness-cli");
        let analysis = analyze_source(&source)
            .unwrap_or_else(|error| panic!("{} should parse as Rust: {error}", path.display()));
        for construction in analysis.direct_constructions {
            if relative == Path::new(allowed_path)
                && construction.type_name == allowed_type
                && construction.associated_item.as_deref() == Some(allowed_item)
                && construction.is_call
            {
                continue;
            }
            violations.push(format!(
                "{} — `{}` constructs `{}` outside harness_agents::builder",
                relative.display(),
                construction.syntax,
                construction.type_name
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "agent backends must be constructed by `harness_agents::builder`, \
         so every entry point gets the same configuration:\n{}",
        violations.join("\n")
    );
}

#[test]
fn syntax_scan_ignores_comment_and_string_bait() {
    let source = r#"
        fn bait() {
            // AgentRegistry::new();
            /* CodexAgent::new(); */
            let _ = "ClaudeCodeAgent::new()";
            let _ = "harness_agents::builder::registry_from_config(";
        }
    "#;

    let analysis = analyze_source(source).expect("bait source should parse");
    assert!(analysis.direct_constructions.is_empty());
    assert_eq!(
        analysis.call_count("harness_agents::builder::registry_from_config"),
        0
    );
}

#[test]
fn syntax_scan_detects_renamed_and_type_aliased_construction() {
    let renamed = r#"
        use harness_agents::registry::AgentRegistry as Registry;

        fn build() {
            let _ = Registry::new();
        }
    "#;
    let type_aliased = r#"
        use harness_agents::codex::CodexAgent;
        type ReviewAgent = CodexAgent;

        fn build() {
            let _ = ReviewAgent::new();
        }
    "#;

    for source in [renamed, type_aliased] {
        let analysis = analyze_source(source).expect("alias source should parse");
        assert_eq!(analysis.direct_constructions.len(), 1);
    }
}

#[test]
fn syntax_scan_detects_struct_literal_construction() {
    let source = r#"
        use harness_agents::codex::CodexAgent as ReviewAgent;

        fn build() {
            let _ = ReviewAgent {
                field: unreachable!(),
            };
        }
    "#;

    let analysis = analyze_source(source).expect("struct literal source should parse");
    assert_eq!(
        analysis.direct_constructions,
        [DirectConstruction {
            type_name: "CodexAgent".to_string(),
            associated_item: None,
            syntax: "ReviewAgent { .. }".to_string(),
            is_call: false,
        }]
    );
}
