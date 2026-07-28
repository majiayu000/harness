//! Structural guard for configured agent construction in `harness-cli`.

#[path = "support/agent_registry_analysis.rs"]
mod analysis;

use analysis::*;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpectedBuilderUse {
    LetBinding(&'static str),
    TailExpression,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectConstruction {
    type_name: String,
    syntax: String,
    module_path: String,
    enclosing_function: Option<String>,
    intentional_pr_review_constructor: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct MacroViolation {
    macro_path: String,
    forbidden_type: String,
}

struct SourceUnit {
    crate_id: String,
    relative: PathBuf,
    module_path: Vec<String>,
    file: syn::File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BuilderCallUse {
    LetBinding(String),
    TailExpression,
    Other,
}

#[derive(Clone, Debug)]
struct BuilderCall {
    path: Vec<String>,
    function: Option<String>,
    usage: BuilderCallUse,
    top_level: bool,
}

const REGISTRY_BUILDER: &str = "harness_agents::builder::registry_from_config";
const CLAUDE_BUILDER: &str = "harness_agents::builder::claude_agent_from_config";
const REQUIRED_BUILDER_CALLS: [(&str, &str, &str, ExpectedBuilderUse); 5] = [
    (
        "src/commands/serve.rs",
        REGISTRY_BUILDER,
        "run",
        ExpectedBuilderUse::LetBinding("agent_registry"),
    ),
    (
        "src/commands/exec.rs",
        REGISTRY_BUILDER,
        "run",
        ExpectedBuilderUse::LetBinding("agent_registry"),
    ),
    (
        "src/gc.rs",
        REGISTRY_BUILDER,
        "build_agent_registry",
        ExpectedBuilderUse::TailExpression,
    ),
    (
        "src/cmd/mcp_server.rs",
        REGISTRY_BUILDER,
        "run",
        ExpectedBuilderUse::LetBinding("agent_registry"),
    ),
    (
        "src/cmd/pr.rs",
        CLAUDE_BUILDER,
        "create_agent",
        ExpectedBuilderUse::TailExpression,
    ),
];
const ALLOWED_DIRECT_CONSTRUCTION_PATH: &str = "src/cmd/pr.rs";

#[test]
fn required_cli_paths_call_the_shared_builder_directly() {
    let analyses = analyze_cli_sources();
    for (relative, expected_call, function, usage) in REQUIRED_BUILDER_CALLS {
        let analysis = analyses
            .get(Path::new(relative))
            .unwrap_or_else(|| panic!("{relative} should be analyzed"));
        assert_eq!(
            analysis.required_builder_call_count(expected_call, function, usage),
            1,
            "{relative}::{function} must use canonical `{expected_call}` exactly once"
        );
    }
}

#[test]
fn no_cli_source_assembles_agent_backends_by_hand() {
    let mut allowed = 0;
    let mut violations = Vec::new();
    for (relative, analysis) in analyze_cli_sources() {
        for construction in analysis.direct_constructions {
            if relative == Path::new(ALLOWED_DIRECT_CONSTRUCTION_PATH)
                && construction.intentional_pr_review_constructor
            {
                allowed += 1;
                continue;
            }
            violations.push(format!(
                "{} — `{}` constructs `{}` in {}::{:?}",
                relative.display(),
                construction.syntax,
                construction.type_name,
                construction.module_path,
                construction.enclosing_function
            ));
        }
        for violation in analysis.macro_violations {
            violations.push(format!(
                "{} — macro `{}` contains potential `{}` construction tokens",
                relative.display(),
                violation.macro_path,
                violation.forbidden_type
            ));
        }
        if analysis.public_glob_reexports > 0 {
            violations.push(format!(
                "{} — contains {} production glob re-export(s), which the alias guard cannot \
                 resolve safely",
                relative.display(),
                analysis.public_glob_reexports
            ));
        }
    }
    assert_eq!(
        allowed, 1,
        "{ALLOWED_DIRECT_CONSTRUCTION_PATH} must contain exactly the intentional read-only \
         CodexAgent::new call in review"
    );
    assert!(
        violations.is_empty(),
        "agent construction must stay in harness_agents::builder:\n{}",
        violations.join("\n")
    );
}

#[test]
fn dead_builder_bait_does_not_satisfy_the_entrypoint_contract() {
    let analysis = analyze_source(
        r#"
        fn run() {
            alternate_registry();
        }
        mod decoy {
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config();
            }
        }
        fn unused_builder_bait() {
            harness_agents::builder::registry_from_config();
        }
        "#,
    )
    .expect("dead-bait source parses");
    assert_eq!(analysis.direct_builder_call_count(REGISTRY_BUILDER), 2);
    assert_eq!(
        analysis.required_builder_call_count(
            REGISTRY_BUILDER,
            "run",
            ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        0
    );
}

#[test]
fn cfg_ambiguous_canonical_builder_path_fails_closed() {
    let analysis = analyze_source(
        r#"
        #[cfg(feature = "external")]
        use ::harness_agents as harness_agents;
        #[cfg(not(feature = "external"))]
        mod harness_agents {
            pub mod builder {
                pub fn registry_from_config() {}
            }
        }
        fn run() {
            harness_agents::builder::registry_from_config();
        }
        "#,
    )
    .expect("cfg-alternative source parses");
    assert_eq!(analysis.direct_builder_call_count(REGISTRY_BUILDER), 0);
}

#[test]
fn production_glob_reexports_fail_closed_without_rejecting_test_preludes() {
    let production = analyze_source(
        r#"
        pub use crate::agent_alias::*;
        "#,
    )
    .expect("production glob parses");
    assert_eq!(production.public_glob_reexports, 1);

    let test_prelude = analyze_source(
        r#"
        #[cfg(test)]
        mod tests {
            use super::*;
        }
        "#,
    )
    .expect("test prelude parses");
    assert_eq!(test_prelude.public_glob_reexports, 0);
}

#[test]
fn binary_roots_resolve_their_own_cross_file_aliases() {
    let analyses = analyze_source_set(&[
        (
            "src/bin/tool.rs",
            r#"
            mod alias;
            use crate::alias::Registry;
            fn main() { let _ = Registry::new("default"); }
            "#,
        ),
        (
            "src/bin/tool/alias.rs",
            "pub type Registry = harness_agents::registry::AgentRegistry;",
        ),
    ])
    .expect("binary source set parses");
    assert_eq!(
        analyses[Path::new("src/bin/tool.rs")]
            .direct_constructions
            .len(),
        1
    );
}

#[test]
fn duplicate_allowed_review_constructors_remain_distinct_occurrences() {
    let analysis = analyze_source(
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            let _first = CodexAgent::new(
                review_config.cli_path.clone(),
                SandboxMode::ReadOnlyWithNetwork,
            );
            let _second = CodexAgent::new(
                review_config.cli_path.clone(),
                SandboxMode::ReadOnlyWithNetwork,
            );
        }
        "#,
    )
    .expect("duplicate review constructors parse");
    assert_eq!(analysis.direct_constructions.len(), 2);
    assert!(analysis
        .direct_constructions
        .iter()
        .all(|construction| construction.intentional_pr_review_constructor));
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable source directory") {
        let path = entry.expect("readable dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

fn analyze_cli_sources() -> std::collections::HashMap<PathBuf, SourceAnalysis> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_sources(&crate_dir.join("src"), &mut paths);
    paths.sort();
    analyze_owned_sources(
        paths
            .into_iter()
            .map(|path| {
                let relative = path
                    .strip_prefix(&crate_dir)
                    .expect("CLI source stays within crate")
                    .to_path_buf();
                let source = std::fs::read_to_string(&path).expect("readable CLI source");
                (relative, source)
            })
            .collect(),
    )
    .expect("CLI sources should parse")
}

fn module_path(relative: &Path) -> Vec<String> {
    let mut components = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let stem = relative
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if !matches!(stem, "main" | "lib" | "mod") {
        components.push(stem.to_string());
    }
    components
}

fn source_layout(relative: &Path) -> (String, Vec<String>) {
    let relative = relative.strip_prefix("src").unwrap_or(relative);
    let Ok(bin_relative) = relative.strip_prefix("bin") else {
        return ("main".to_string(), module_path(relative));
    };
    let mut parts = bin_relative.components();
    let Some(first) = parts.next() else {
        return ("main".to_string(), module_path(relative));
    };
    let first = PathBuf::from(first.as_os_str());
    if parts.next().is_none() {
        let name = first
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        return (format!("bin:{name}"), Vec::new());
    }
    let name = first.to_string_lossy().into_owned();
    let within_binary = bin_relative.strip_prefix(&first).unwrap_or(bin_relative);
    (format!("bin:{name}"), module_path(within_binary))
}

fn analyze_source(source: &str) -> syn::Result<SourceAnalysis> {
    Ok(analyze_source_set(&[("src/main.rs", source)])?
        .remove(Path::new("src/main.rs"))
        .expect("single source analysis exists"))
}

fn analyze_source_set(
    sources: &[(&str, &str)],
) -> syn::Result<std::collections::HashMap<PathBuf, SourceAnalysis>> {
    analyze_owned_sources(
        sources
            .iter()
            .map(|(relative, source)| (PathBuf::from(relative), (*source).to_string()))
            .collect(),
    )
}

fn analyze_owned_sources(
    sources: Vec<(PathBuf, String)>,
) -> syn::Result<std::collections::HashMap<PathBuf, SourceAnalysis>> {
    let mut units = Vec::new();
    for (relative, source) in sources {
        let (crate_id, module_path) = source_layout(&relative);
        units.push(SourceUnit {
            crate_id,
            module_path,
            relative,
            file: syn::parse_file(&source)?,
        });
    }
    Ok(analyze_units(&units))
}

#[test]
fn syntax_scan_ignores_comments_and_string_literals() {
    let analysis = analyze_source(
        r#"
        fn bait() {
            // AgentRegistry::new();
            /* CodexAgent::new(); */
            let _ = "ClaudeCodeAgent::new()";
            stringify!("ProviderBackpressureGate::default()");
        }
        "#,
    )
    .expect("bait source parses");
    assert!(analysis.direct_constructions.is_empty());
    assert!(analysis.macro_violations.is_empty());
}

#[test]
fn scoped_aliases_respect_sibling_and_local_shadowing_in_both_orders() {
    let analysis = analyze_source(
        r#"
        mod forbidden_then_harmless {
            mod a {
                use harness_agents::registry::AgentRegistry as Registry;
                fn build() { let _ = Registry::new("default"); }
            }
            mod b {
                use harmless::Thing as Registry;
                fn harmless() { let _ = Registry::new(); }
            }
        }
        mod harmless_then_forbidden {
            mod a {
                use harmless::Thing as Registry;
                fn harmless() { let _ = Registry::new(); }
            }
            mod b {
                use harness_agents::registry::AgentRegistry as Registry;
                fn build() { let _ = Registry::new("default"); }
            }
        }
        fn forbidden_local_alias() {
            type Registry = harness_agents::registry::AgentRegistry;
            let _ = Registry::new("default");
        }
        fn harmless_local_alias() {
            type Registry = harmless::Thing;
            let _ = Registry::new();
        }
        mod reversed_local_aliases {
            fn harmless() {
                type Registry = harmless::Thing;
                let _ = Registry::new();
            }
            fn forbidden() {
                type Registry = harness_agents::registry::AgentRegistry;
                let _ = Registry::new("default");
            }
        }
        "#,
    )
    .expect("scoped aliases parse");
    assert_eq!(
        analysis.direct_constructions.len(),
        4,
        "constructions: {:?}",
        analysis.direct_constructions
    );
    assert!(analysis
        .direct_constructions
        .iter()
        .all(|construction| construction.type_name == "AgentRegistry"));
    let locations = analysis
        .direct_constructions
        .iter()
        .map(|construction| {
            (
                construction.module_path.as_str(),
                construction.enclosing_function.as_deref(),
            )
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        locations,
        std::collections::HashSet::from([
            ("forbidden_then_harmless::a", Some("build")),
            ("harmless_then_forbidden::b", Some("build")),
            ("", Some("forbidden_local_alias")),
            ("reversed_local_aliases", Some("forbidden")),
        ])
    );
}

#[test]
fn cross_file_reexports_resolve_to_forbidden_types() {
    let analyses = analyze_source_set(&[
        (
            "src/agent_alias.rs",
            r#"
            pub use harness_agents::registry::AgentRegistry as Registry;
            pub type RegistryType = Registry;
            "#,
        ),
        (
            "src/consumer.rs",
            r#"
            use crate::agent_alias::RegistryType;
            fn build() { let _ = RegistryType::new("default"); }
            "#,
        ),
    ])
    .expect("cross-file aliases parse");
    assert_eq!(
        analyses[Path::new("src/consumer.rs")]
            .direct_constructions
            .len(),
        1
    );
}

#[test]
fn builder_aliases_and_reexports_do_not_count_as_direct_calls() {
    let analyses = analyze_source_set(&[
        (
            "src/build_alias.rs",
            "pub use harness_agents::builder::registry_from_config as build;",
        ),
        (
            "src/main.rs",
            r#"
            use harness_agents::builder::registry_from_config as aliased;
            use crate::build_alias::build;
            fn bait() {
                aliased();
                build();
                harness_agents::builder::registry_from_config();
            }
            "#,
        ),
    ])
    .expect("builder aliases parse");
    assert_eq!(
        analyses[Path::new("src/main.rs")]
            .direct_builder_call_count("harness_agents::builder::registry_from_config"),
        1
    );

    for bait in [
        r#"
        use harmless as harness_agents;
        fn bait() { harness_agents::builder::registry_from_config(); }
        "#,
        r#"
        mod harness_agents {
            mod builder { fn registry_from_config() {} }
        }
        fn bait() { harness_agents::builder::registry_from_config(); }
        "#,
    ] {
        let analysis = analyze_source(bait).expect("shadowed canonical path parses");
        assert_eq!(
            analysis.direct_builder_call_count("harness_agents::builder::registry_from_config"),
            0
        );
    }
}

#[test]
fn macro_tokens_fail_closed_without_literal_false_positives() {
    let analysis = analyze_source(
        r#"
        use harness_agents::registry::AgentRegistry as Registry;
        macro_rules! direct { () => { Registry::new("default") }; }
        macro_rules! via_self { () => { Self::new("default") }; }
        fn unused_builder_bait() {
            harness_agents::builder::registry_from_config();
        }
        fn invoke() {
            direct!();
            some_macro!(harness_agents::codex::CodexAgent::new());
            some_macro!("ClaudeCodeAgent::new()");
        }
        "#,
    )
    .expect("macro source parses");
    assert_eq!(
        analysis.direct_builder_call_count("harness_agents::builder::registry_from_config"),
        1
    );
    assert_eq!(analysis.macro_violations.len(), 3);
    assert!(analysis
        .macro_violations
        .iter()
        .any(|violation| violation.forbidden_type == "AgentRegistry"));
    assert!(analysis
        .macro_violations
        .iter()
        .any(|violation| violation.forbidden_type == "CodexAgent"));
    assert!(analysis
        .macro_violations
        .iter()
        .any(|violation| violation.forbidden_type == "unresolved Self"));
}

#[test]
fn constructor_references_calls_struct_tuple_unit_and_self_are_detected() {
    let analysis = analyze_source(
        r#"
        use harness_agents::registry::AgentRegistry as Registry;
        use harness_agents::registry::AgentRegistry as r#RawRegistry;
        use harness_agents::codex::CodexAgent as ReviewAgent;
        trait Build { fn build() -> Registry; }
        impl Build for Registry {
            fn build() -> Self { Self::new("default") }
        }
        fn forms() {
            let _reference = Registry::new;
            let _call = Registry::new("default");
            let _raw = r#RawRegistry::new("default");
            let _struct = ReviewAgent { field: unreachable!() };
            let _tuple = ReviewAgent(unreachable!());
            let _unit = ReviewAgent;
        }
        "#,
    )
    .expect("constructor forms parse");
    assert_eq!(analysis.direct_constructions.len(), 7);
}

#[test]
fn codex_exception_is_anchored_to_review_and_read_only_arguments() {
    let valid = analyze_source(
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            CodexAgent::new(review_config.cli_path.clone(), SandboxMode::ReadOnlyWithNetwork);
        }
        "#,
    )
    .expect("valid exception parses");
    assert!(valid.direct_constructions[0].intentional_pr_review_constructor);

    for invalid in [
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn fix() {
            CodexAgent::new(review_config.cli_path.clone(), SandboxMode::ReadOnlyWithNetwork);
        }
        "#,
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            CodexAgent::new(review_config.cli_path.clone(), SandboxMode::DangerFullAccess);
        }
        "#,
        r#"
        use harness_agents::codex::CodexAgent as ReviewAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            ReviewAgent::new(
                review_config.cli_path.clone(),
                SandboxMode::ReadOnlyWithNetwork,
            );
        }
        "#,
    ] {
        let analysis = analyze_source(invalid).expect("invalid exception parses");
        assert!(!analysis.direct_constructions[0].intentional_pr_review_constructor);
    }
}
