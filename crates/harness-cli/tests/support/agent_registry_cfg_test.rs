use super::{resolution::Segments, SourceUnit};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use syn::{
    parse::Parser, punctuated::Punctuated, Attribute, Expr, ExprLit, Item, Lit, Meta, Token,
};

pub(super) fn has_cfg_test(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Ident>()
                .is_ok_and(|condition| condition == "test")
    })
}

pub(super) fn has_cfg_gate(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            || (attribute.path().is_ident("cfg_attr")
                && match &attribute.meta {
                    Meta::List(list) => cfg_attr_contains_cfg_gate(list.tokens.clone()),
                    Meta::Path(_) | Meta::NameValue(_) => true,
                })
    })
}

fn cfg_attr_contains_cfg_gate(tokens: proc_macro2::TokenStream) -> bool {
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(arguments) = parser.parse2(tokens) else {
        return true;
    };
    arguments.iter().skip(1).any(meta_contains_cfg_gate)
}

fn meta_contains_cfg_gate(meta: &Meta) -> bool {
    if meta.path().is_ident("cfg") {
        return true;
    }
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    match meta {
        Meta::List(list) => cfg_attr_contains_cfg_gate(list.tokens.clone()),
        Meta::Path(_) | Meta::NameValue(_) => true,
    }
}

#[derive(Default)]
pub(super) struct CfgTestModules {
    module_paths_by_crate: HashMap<String, HashSet<Segments>>,
    source_paths_by_crate: HashMap<String, HashSet<PathBuf>>,
}

impl CfgTestModules {
    pub(super) fn collect(units: &[SourceUnit]) -> Self {
        let mut modules = Self::default();
        for unit in units {
            collect_cfg_test_modules(
                &unit.file.items,
                &unit.module_path,
                &unit.relative,
                false,
                modules
                    .module_paths_by_crate
                    .entry(unit.crate_id.clone())
                    .or_default(),
                modules
                    .source_paths_by_crate
                    .entry(unit.crate_id.clone())
                    .or_default(),
            );
        }
        modules
    }

    pub(super) fn contains(&self, unit: &SourceUnit) -> bool {
        self.module_paths_by_crate
            .get(&unit.crate_id)
            .is_some_and(|test_modules| {
                test_modules
                    .iter()
                    .any(|test_module| unit.module_path.starts_with(test_module))
            })
            || self
                .source_paths_by_crate
                .get(&unit.crate_id)
                .is_some_and(|test_sources| test_sources.contains(&unit.relative))
    }
}

fn collect_cfg_test_modules(
    items: &[Item],
    module_path: &[String],
    source_path: &Path,
    inherited_test_context: bool,
    test_modules: &mut HashSet<Segments>,
    test_sources: &mut HashSet<PathBuf>,
) {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let is_test_module = inherited_test_context || has_cfg_test(&module.attrs);
        let mut nested_path = module_path.to_vec();
        nested_path.push(super::resolution::ident_name(&module.ident));
        if is_test_module {
            test_modules.insert(nested_path.clone());
            if module.content.is_none() {
                if let Some(path) = module_source_path(&module.attrs, source_path) {
                    test_sources.insert(path);
                }
            }
        }
        if let Some((_, nested)) = &module.content {
            collect_cfg_test_modules(
                nested,
                &nested_path,
                source_path,
                is_test_module,
                test_modules,
                test_sources,
            );
        }
    }
}

fn module_source_path(attributes: &[Attribute], source_path: &Path) -> Option<PathBuf> {
    let relative = attributes.iter().find_map(|attribute| {
        if !attribute.path().is_ident("path") {
            return None;
        }
        let Meta::NameValue(meta) = &attribute.meta else {
            return None;
        };
        let Expr::Lit(ExprLit {
            lit: Lit::Str(path),
            ..
        }) = &meta.value
        else {
            return None;
        };
        Some(PathBuf::from(path.value()))
    })?;
    Some(
        source_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(relative),
    )
}

#[test]
fn entrypoint_contract_rejects_cfg_gated_builder_bait() {
    let function_bait = super::analyze_source(
        r#"
        #[cfg(test)]
        fn run() {
            let agent_registry =
                harness_agents::builder::registry_from_config();
        }
        #[cfg(not(test))]
        fn run() {
            alternate_registry();
        }
        "#,
    )
    .expect("cfg-gated function bait parses");
    assert_eq!(
        function_bait.required_builder_call_count(
            super::REGISTRY_BUILDER,
            "run",
            super::ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        0,
        "a test-only function must not satisfy the production entrypoint contract"
    );

    let statement_bait = super::analyze_source(
        r#"
        fn run() {
            #[cfg(test)]
            let agent_registry =
                harness_agents::builder::registry_from_config();
            #[cfg(not(test))]
            let agent_registry = alternate_registry();
            consume(agent_registry);
        }
        "#,
    )
    .expect("cfg-gated statement bait parses");
    assert_eq!(
        statement_bait.required_builder_call_count(
            super::REGISTRY_BUILDER,
            "run",
            super::ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        0,
        "a test-only statement must not satisfy the production entrypoint contract"
    );

    let cfg_attr_bait = super::analyze_source(
        r#"
        #[cfg_attr(not(test), cfg(test))]
        fn run() {
            let agent_registry =
                harness_agents::builder::registry_from_config();
        }
        "#,
    )
    .expect("nested cfg gate parses");
    assert_eq!(
        cfg_attr_bait.required_builder_call_count(
            super::REGISTRY_BUILDER,
            "run",
            super::ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        0,
        "a cfg_attr-applied cfg gate must fail closed"
    );
}

#[test]
fn harmless_attributes_do_not_hide_required_builder_calls() {
    let analysis = super::analyze_source(
        r#"
        #[allow(dead_code)]
        #[cfg_attr(test, allow(unused_variables))]
        fn run() {
            #[allow(unused_variables)]
            let agent_registry =
                harness_agents::builder::registry_from_config();
        }
        "#,
    )
    .expect("harmless attributes parse");
    assert_eq!(
        analysis.required_builder_call_count(
            super::REGISTRY_BUILDER,
            "run",
            super::ExpectedBuilderUse::LetBinding("agent_registry"),
        ),
        1,
        "non-gating attributes must not invalidate a production builder call"
    );
}
