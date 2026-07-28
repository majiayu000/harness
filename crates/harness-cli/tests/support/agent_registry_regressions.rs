use super::*;

#[test]
fn entrypoint_contract_rejects_dead_nested_scopes() {
    for bait in [
        r#"
        fn run() {
            if false {
                let agent_registry =
                    harness_agents::builder::registry_from_config();
            }
            alternate_registry();
        }
        "#,
        r#"
        fn run() {
            let _factory = || {
                let agent_registry =
                    harness_agents::builder::registry_from_config();
            };
            alternate_registry();
        }
        "#,
        r#"
        fn run() {
            alternate_registry();
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config();
            }
        }
        "#,
    ] {
        let analysis = analyze_source(bait).expect("nested builder bait parses");
        assert_eq!(
            analysis.required_builder_call_count(
                REGISTRY_BUILDER,
                "run",
                ExpectedBuilderUse::LetBinding("agent_registry"),
            ),
            0
        );
    }

    let tail_bait = analyze_source(
        r#"
        fn build_agent_registry() {
            fn build_agent_registry() {
                harness_agents::builder::registry_from_config()
            }
            alternate_registry()
        }
        "#,
    )
    .expect("nested tail bait parses");
    assert_eq!(
        tail_bait.required_builder_call_count(
            REGISTRY_BUILDER,
            "build_agent_registry",
            ExpectedBuilderUse::TailExpression,
        ),
        0
    );
}

#[test]
fn entrypoint_contract_rejects_const_item_function_bait() {
    let analysis = analyze_source(
        r#"
        const _: () = {
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config();
            }
        };
        fn run() {
            alternate_registry();
        }
        "#,
    )
    .expect("const-item builder bait parses");

    assert_eq!(
        analysis.required_builder_call_count(
            REGISTRY_BUILDER,
            "run",
            ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        0,
        "a function item nested in a const block is not a top-level entry point"
    );
}

#[test]
fn direct_typed_binding_still_satisfies_entrypoint_contract() {
    let analysis = analyze_source(
        r#"
        fn run() {
            let agent_registry: AgentRegistry =
                harness_agents::builder::registry_from_config();
        }
        "#,
    )
    .expect("typed direct binding parses");
    assert_eq!(
        analysis.required_builder_call_count(
            REGISTRY_BUILDER,
            "run",
            ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        1
    );
}

#[test]
fn review_exception_rejects_dead_nested_scopes() {
    for bait in [
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn outer() {
            fn review() {
                CodexAgent::new(
                    review_config.cli_path.clone(),
                    SandboxMode::ReadOnlyWithNetwork,
                );
            }
        }
        "#,
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            if false {
                CodexAgent::new(
                    review_config.cli_path.clone(),
                    SandboxMode::ReadOnlyWithNetwork,
                );
            }
        }
        "#,
        r#"
        use harness_agents::codex::CodexAgent;
        use harness_core::config::agents::SandboxMode;
        fn review() {
            let _factory = || CodexAgent::new(
                review_config.cli_path.clone(),
                SandboxMode::ReadOnlyWithNetwork,
            );
        }
        "#,
    ] {
        let analysis = analyze_source(bait).expect("nested review bait parses");
        assert_eq!(analysis.direct_constructions.len(), 1);
        assert!(!analysis.direct_constructions[0].intentional_pr_review_constructor);
    }
}

#[test]
fn all_production_globs_fail_closed_but_external_test_preludes_do_not() {
    let private_glob = analyze_source(
        r#"
        mod agent_alias {
            pub use harness_agents::registry::AgentRegistry as Registry;
        }
        use crate::agent_alias::*;
        fn build() { let _ = Registry::new("default"); }
        "#,
    )
    .expect("private production glob parses");
    assert_eq!(private_glob.production_glob_imports, 1);

    let test_sources = analyze_source_set(&[
        ("src/main.rs", "#[cfg(test)] mod tests;"),
        ("src/tests.rs", "use super::*;"),
    ])
    .expect("external test module parses");
    assert_eq!(
        test_sources[Path::new("src/tests.rs")].production_glob_imports,
        0
    );

    let non_super_test_glob = analyze_source(
        r#"
        #[cfg(test)]
        mod tests {
            use crate::agent_alias::*;
        }
        "#,
    )
    .expect("non-super test glob parses");
    assert_eq!(non_super_test_glob.production_glob_imports, 1);
}

#[test]
fn path_attributed_cfg_test_module_keeps_its_private_super_prelude() {
    let test_sources = analyze_source_set(&[
        (
            "src/main.rs",
            r#"
            #[cfg(test)]
            #[path = "main_tests.rs"]
            mod tests;
            "#,
        ),
        ("src/main_tests.rs", "use super::*;"),
    ])
    .expect("path-attributed test module parses");

    assert_eq!(
        test_sources[Path::new("src/main_tests.rs")].production_glob_imports,
        0,
        "a private super prelude in a path-attributed cfg(test) module is test-only"
    );
}

#[test]
fn typed_trait_constructions_are_detected_without_qself_duplicates() {
    for source in [
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn build() {
            let _gate: ProviderBackpressureGate = Default::default();
        }
        "#,
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn build() -> ProviderBackpressureGate {
            Default::default()
        }
        "#,
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn build() {
            let _gate =
                <ProviderBackpressureGate as Default>::default();
        }
        "#,
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        struct Factory;
        impl Factory {
            fn build() -> ProviderBackpressureGate {
                Default::default()
            }
        }
        "#,
    ] {
        let analysis = analyze_source(source).expect("typed construction parses");
        assert_eq!(
            analysis.direct_constructions.len(),
            1,
            "constructions: {:?}",
            analysis.direct_constructions
        );
    }
}

#[test]
fn explicit_typed_returns_are_detected_in_free_and_impl_functions() {
    for source in [
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn build() -> ProviderBackpressureGate {
            return Default::default();
        }
        "#,
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        struct Factory;
        impl Factory {
            fn build() -> ProviderBackpressureGate {
                return Default::default();
            }
        }
        "#,
    ] {
        let analysis = analyze_source(source).expect("explicit typed return parses");
        assert_eq!(
            analysis.direct_constructions.len(),
            1,
            "constructions: {:?}",
            analysis.direct_constructions
        );
    }
}

#[test]
fn generic_factory_turbofish_detects_output_without_observation_false_positives() {
    let construction = analyze_source(
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn build() {
            fn make<'a, T: Default>() -> T { T::default() }
            let _gate = make::<ProviderBackpressureGate>();
        }
        "#,
    )
    .expect("generic factory parses");
    assert_eq!(construction.direct_constructions.len(), 1);

    let observation = analyze_source(
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn inspect<T>() -> &'static str { std::any::type_name::<T>() }
        fn identity<T>(value: T) -> T { value }
        fn observe(gate: ProviderBackpressureGate) {
            let _ = inspect::<ProviderBackpressureGate>();
            let _ = std::mem::size_of::<ProviderBackpressureGate>();
            let _ = identity::<ProviderBackpressureGate>(gate);
        }
        "#,
    )
    .expect("generic observation parses");
    assert!(observation.direct_constructions.is_empty());
}

#[test]
fn generic_factory_resolution_covers_impls_without_merging_local_scopes() {
    let associated = analyze_source(
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        struct Factory;
        impl Factory {
            fn make<T: Default>() -> T { T::default() }
        }
        fn build() {
            let _gate = Factory::make::<ProviderBackpressureGate>();
        }
        "#,
    )
    .expect("associated generic factory parses");

    let lexical = analyze_source(
        r#"
        use harness_agents::provider_backpressure::ProviderBackpressureGate;
        fn first() {
            fn make<T, U>() -> T { loop {} }
            let _value: u8 = make::<u8, ProviderBackpressureGate>();
        }
        fn second() {
            fn make<T, U>() -> U { loop {} }
        }
        "#,
    )
    .expect("lexically distinct generic factories parse");

    assert_eq!(
        (
            associated.direct_constructions.len(),
            lexical.direct_constructions.len(),
        ),
        (1, 0),
        "associated factories must be detected without merging same-name local factories"
    );
}
