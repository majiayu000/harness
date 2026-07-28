//! Structural guard for configured agent construction in `harness-cli`.

#[path = "support/agent_registry_analysis.rs"]
mod analysis;

use analysis::*;
use std::path::Path;

#[test]
fn required_cli_paths_call_the_shared_builder_directly() {
    let analyses = analyze_cli_sources();
    for (relative, expected_call) in REQUIRED_BUILDER_CALLS {
        let analysis = analyses
            .get(Path::new(relative))
            .unwrap_or_else(|| panic!("{relative} should be analyzed"));
        assert_eq!(
            analysis.direct_builder_call_count(expected_call),
            1,
            "{relative} must directly invoke canonical `{expected_call}` exactly once"
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
