use std::collections::{HashMap, HashSet, VecDeque};
use syn::{
    visit::{self, Visit},
    Block, GenericArgument, GenericParam, Item, ItemFn, ItemMod, ItemType, ItemUse,
    Path as SynPath, ReturnType, Stmt, Type, TypePath, UseTree,
};

pub(super) type Segments = Vec<String>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AliasTarget {
    path: Segments,
    module_path: Segments,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AliasScope {
    paths: HashMap<String, Vec<AliasTarget>>,
}

impl AliasScope {
    fn insert(&mut self, name: String, target: AliasTarget) {
        let targets = self.paths.entry(name).or_default();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct CrateAliases {
    paths: HashMap<Segments, Vec<Segments>>,
}

impl CrateAliases {
    fn insert(&mut self, name: Segments, target: Segments) {
        let targets = self.paths.entry(name).or_default();
        if !targets.contains(&target) {
            targets.push(target);
        }
    }

    fn longest_match(&self, path: &[String]) -> Option<(usize, &[Segments])> {
        (1..=path.len()).rev().find_map(|length| {
            self.paths
                .get(&path[..length])
                .map(|targets| (length, &targets[..]))
        })
    }
}

pub(super) struct Resolver<'a> {
    crate_aliases: &'a CrateAliases,
    scopes: &'a [AliasScope],
    module_path: &'a [String],
    impl_types: &'a [Segments],
}

impl<'a> Resolver<'a> {
    pub(super) fn new(
        crate_aliases: &'a CrateAliases,
        scopes: &'a [AliasScope],
        module_path: &'a [String],
        impl_types: &'a [Segments],
    ) -> Self {
        Self {
            crate_aliases,
            scopes,
            module_path,
            impl_types,
        }
    }

    pub(super) fn resolve(&self, mut path: Segments) -> Vec<Segments> {
        if path.first().is_some_and(|segment| segment == "Self") {
            if let Some(impl_type) = self.impl_types.last() {
                let mut expanded = impl_type.clone();
                expanded.extend(path.into_iter().skip(1));
                path = expanded;
            }
        }

        let mut queue = VecDeque::from([normalize_relative(path, self.module_path)]);
        let mut visited = HashSet::new();
        let mut resolved = HashSet::new();

        while let Some(candidate) = queue.pop_front() {
            if !visited.insert(candidate.clone()) {
                resolved.insert(candidate);
                continue;
            }

            if let Some(first) = candidate.first() {
                if let Some(targets) = self
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.paths.get(first))
                {
                    for target in targets {
                        let mut expanded =
                            normalize_relative(target.path.clone(), &target.module_path);
                        expanded.extend(candidate.iter().skip(1).cloned());
                        queue.push_back(expanded);
                    }
                    continue;
                }
            }

            if let Some((prefix_length, targets)) = self.crate_aliases.longest_match(&candidate) {
                for target in targets {
                    let mut expanded = target.clone();
                    expanded.extend(candidate.iter().skip(prefix_length).cloned());
                    queue.push_back(expanded);
                }
                continue;
            }

            resolved.insert(candidate);
        }

        resolved.into_iter().collect()
    }
}

#[derive(Debug, Default)]
pub(super) struct GenericFactories {
    output_parameters_by_path: HashMap<Segments, Vec<usize>>,
}

impl GenericFactories {
    pub(super) fn output_type_arguments<'a>(
        &self,
        path: &'a SynPath,
        resolved_paths: &[Segments],
        module_path: &[String],
    ) -> Vec<&'a Type> {
        let original = path_segments(path);
        let mut candidates = resolved_paths.to_vec();
        if path.leading_colon.is_none()
            && original
                .first()
                .is_some_and(|segment| !matches!(segment.as_str(), "crate" | "self" | "super"))
        {
            let mut local = vec!["crate".to_string()];
            local.extend(module_path.iter().cloned());
            local.extend(original);
            if !candidates.contains(&local) {
                candidates.push(local);
            }
        }

        let Some(arguments) = path.segments.last().and_then(|segment| {
            let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                return None;
            };
            Some(arguments)
        }) else {
            return Vec::new();
        };

        let mut output_types: Vec<&'a Type> = Vec::new();
        for candidate in candidates {
            let Some(positions) = self.output_parameters_by_path.get(&candidate) else {
                continue;
            };
            for position in positions {
                let Some(GenericArgument::Type(output_type)) = arguments.args.iter().nth(*position)
                else {
                    continue;
                };
                if !output_types
                    .iter()
                    .any(|existing| std::ptr::eq(*existing, output_type))
                {
                    output_types.push(output_type);
                }
            }
        }
        output_types
    }
}

pub(super) fn path_segments(path: &SynPath) -> Segments {
    path.segments
        .iter()
        .map(|segment| ident_name(&segment.ident))
        .collect()
}

pub(super) fn ident_name(ident: &proc_macro2::Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_string()
}

pub(super) fn type_paths(type_: &Type) -> Vec<Segments> {
    #[derive(Default)]
    struct TypePathCollector {
        paths: Vec<Segments>,
    }

    impl<'ast> Visit<'ast> for TypePathCollector {
        fn visit_type_path(&mut self, type_path: &'ast TypePath) {
            if type_path.qself.is_none() {
                self.paths.push(path_segments(&type_path.path));
            }
            visit::visit_type_path(self, type_path);
        }
    }

    let mut collector = TypePathCollector::default();
    collector.visit_type(type_);
    collector.paths
}

fn normalize_relative(mut path: Segments, module_path: &[String]) -> Segments {
    if path.first().is_some_and(|segment| segment == "crate") {
        return path;
    }
    if path.first().is_some_and(|segment| segment == "self") {
        let mut normalized = vec!["crate".to_string()];
        normalized.extend(module_path.iter().cloned());
        normalized.extend(path.into_iter().skip(1));
        return normalized;
    }
    if path.first().is_some_and(|segment| segment == "super") {
        let mut module = module_path.to_vec();
        while path.first().is_some_and(|segment| segment == "super") {
            module.pop();
            path.remove(0);
        }
        let mut normalized = vec!["crate".to_string()];
        normalized.extend(module);
        normalized.extend(path);
        return normalized;
    }
    path
}

fn collect_use_aliases(
    tree: &UseTree,
    prefix: &mut Segments,
    aliases: &mut AliasScope,
    module_path: &[String],
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_name(&path.ident));
            collect_use_aliases(&path.tree, prefix, aliases, module_path);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let imported = ident_name(&name.ident);
            if imported == "self" {
                if let Some(local_name) = prefix.last() {
                    aliases.insert(
                        local_name.clone(),
                        AliasTarget {
                            path: prefix.clone(),
                            module_path: module_path.to_vec(),
                        },
                    );
                }
            } else {
                let mut target = prefix.clone();
                target.push(imported.clone());
                aliases.insert(
                    imported,
                    AliasTarget {
                        path: target,
                        module_path: module_path.to_vec(),
                    },
                );
            }
        }
        UseTree::Rename(rename) => {
            let imported = ident_name(&rename.ident);
            let mut target = prefix.clone();
            if imported != "self" {
                target.push(imported);
            }
            aliases.insert(
                ident_name(&rename.rename),
                AliasTarget {
                    path: target,
                    module_path: module_path.to_vec(),
                },
            );
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_aliases(item, prefix, aliases, module_path);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn add_item_alias(item: &Item, scope: &mut AliasScope, module_path: &[String]) {
    match item {
        Item::Use(ItemUse { tree, .. }) => {
            collect_use_aliases(tree, &mut Vec::new(), scope, module_path);
        }
        Item::Type(ItemType { ident, ty, .. }) => {
            if let Type::Path(type_path) = ty.as_ref() {
                if type_path.qself.is_none() {
                    scope.insert(
                        ident_name(ident),
                        AliasTarget {
                            path: path_segments(&type_path.path),
                            module_path: module_path.to_vec(),
                        },
                    );
                }
            }
        }
        Item::Mod(ItemMod { ident, .. }) => {
            let mut target = vec!["crate".to_string()];
            target.extend(module_path.iter().cloned());
            target.push(ident_name(ident));
            scope.insert(
                ident_name(ident),
                AliasTarget {
                    path: target,
                    module_path: module_path.to_vec(),
                },
            );
        }
        Item::ExternCrate(item) => {
            let local_name = item
                .rename
                .as_ref()
                .map_or_else(|| ident_name(&item.ident), |(_, ident)| ident_name(ident));
            scope.insert(
                local_name,
                AliasTarget {
                    path: vec![ident_name(&item.ident)],
                    module_path: module_path.to_vec(),
                },
            );
        }
        _ => {}
    }
}

pub(super) fn alias_scope_from_items(items: &[Item], module_path: &[String]) -> AliasScope {
    let mut scope = AliasScope::default();
    for item in items {
        add_item_alias(item, &mut scope, module_path);
    }
    scope
}

pub(super) fn alias_scope_from_block(block: &Block, module_path: &[String]) -> AliasScope {
    let mut scope = AliasScope::default();
    for statement in &block.stmts {
        if let Stmt::Item(item) = statement {
            add_item_alias(item, &mut scope, module_path);
        }
    }
    scope
}

fn expand_in_scope(path: Segments, scope: &AliasScope, module_path: &[String]) -> Vec<Segments> {
    let mut queue = VecDeque::from([path]);
    let mut visited = HashSet::new();
    let mut expanded = HashSet::new();
    while let Some(candidate) = queue.pop_front() {
        if !visited.insert(candidate.clone()) {
            expanded.insert(normalize_relative(candidate, module_path));
            continue;
        }
        if let Some(targets) = candidate.first().and_then(|first| scope.paths.get(first)) {
            for target in targets {
                let mut replacement = normalize_relative(target.path.clone(), &target.module_path);
                replacement.extend(candidate.iter().skip(1).cloned());
                queue.push_back(replacement);
            }
        } else {
            expanded.insert(normalize_relative(candidate, module_path));
        }
    }
    expanded.into_iter().collect()
}

pub(super) fn collect_crate_aliases(
    items: &[Item],
    module_path: &[String],
    aliases: &mut CrateAliases,
) {
    let scope = alias_scope_from_items(items, module_path);
    for (local_name, targets) in &scope.paths {
        let mut absolute_name = vec!["crate".to_string()];
        absolute_name.extend(module_path.iter().cloned());
        absolute_name.push(local_name.clone());
        for target in targets {
            for expanded in expand_in_scope(target.path.clone(), &scope, module_path) {
                aliases.insert(absolute_name.clone(), expanded);
            }
        }
    }

    for item in items {
        if let Item::Mod(ItemMod {
            ident,
            content: Some((_, nested)),
            ..
        }) = item
        {
            let mut nested_module = module_path.to_vec();
            nested_module.push(ident_name(ident));
            collect_crate_aliases(nested, &nested_module, aliases);
        }
    }
}

fn generic_factory_output_position(item: &ItemFn) -> Option<usize> {
    if !item.sig.inputs.is_empty() {
        return None;
    }
    let ReturnType::Type(_, output) = &item.sig.output else {
        return None;
    };
    let Type::Path(output) = output.as_ref() else {
        return None;
    };
    if output.qself.is_some() || output.path.segments.len() != 1 {
        return None;
    }
    let output_name = ident_name(&output.path.segments[0].ident);
    let mut argument_position = 0;
    for parameter in &item.sig.generics.params {
        match parameter {
            GenericParam::Lifetime(_) => continue,
            GenericParam::Type(type_parameter)
                if ident_name(&type_parameter.ident) == output_name =>
            {
                return Some(argument_position);
            }
            GenericParam::Type(_) | GenericParam::Const(_) => argument_position += 1,
        }
    }
    None
}

pub(super) fn collect_generic_factories(
    items: &[Item],
    module_path: &[String],
    factories: &mut GenericFactories,
) {
    struct FactoryCollector<'a> {
        module_path: Segments,
        factories: &'a mut GenericFactories,
    }

    impl<'ast> Visit<'ast> for FactoryCollector<'_> {
        fn visit_item_fn(&mut self, function: &'ast ItemFn) {
            if let Some(position) = generic_factory_output_position(function) {
                let mut path = vec!["crate".to_string()];
                path.extend(self.module_path.iter().cloned());
                path.push(ident_name(&function.sig.ident));
                let positions = self
                    .factories
                    .output_parameters_by_path
                    .entry(path)
                    .or_default();
                if !positions.contains(&position) {
                    positions.push(position);
                }
            }
            visit::visit_item_fn(self, function);
        }

        fn visit_item_mod(&mut self, module: &'ast ItemMod) {
            let Some((_, nested)) = &module.content else {
                return;
            };
            self.module_path.push(ident_name(&module.ident));
            for item in nested {
                self.visit_item(item);
            }
            self.module_path.pop();
        }
    }

    let mut collector = FactoryCollector {
        module_path: module_path.to_vec(),
        factories,
    };
    for item in items {
        collector.visit_item(item);
    }
}
