use super::{resolution::Segments, SourceUnit};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use syn::{Attribute, Expr, ExprLit, Item, Lit, Meta};

pub(super) fn has_cfg_test(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .parse_args::<syn::Ident>()
                .is_ok_and(|condition| condition == "test")
    })
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
