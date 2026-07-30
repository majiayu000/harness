use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use syn::visit::{self, Visit};

#[path = "rest_dto_boundary/syntax.rs"]
mod syntax;
use syntax::*;

const LEGACY_SERVER_LOCAL_REST_DTOS: &str =
    include_str!("fixtures/rest_dto_boundary_allowlist.txt");

#[test]
fn new_rest_dtos_are_not_added_in_server_modules() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sources = load_sources(&manifest_dir);
    let type_index = TypeIndex::new(&sources);
    let mut direct_use_sites = BTreeSet::new();

    for source in &sources {
        let imports = collect_imports(&source.syntax, &type_index);
        RestUseSiteVisitor {
            source,
            imports: &imports,
            type_index: &type_index,
            local_types: BTreeMap::new(),
            raw_body_bindings: BTreeSet::new(),
            discovered: &mut direct_use_sites,
        }
        .visit_file(&source.syntax);
    }

    let discovered = expand_transitive_rest_dtos(direct_use_sites, &type_index);
    let expected = LEGACY_SERVER_LOCAL_REST_DTOS
        .split_whitespace()
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        discovered, expected,
        "new REST request/response DTOs must be defined in harness-protocol::rest, not server-local HTTP modules; update this legacy allowlist only when a DTO is migrated out of harness-server"
    );
}

#[derive(Clone)]
struct SourceFile {
    path_str: String,
    syntax: syn::File,
}

#[derive(Clone, Debug)]
struct SerdeTypeDef {
    path_str: String,
    field_refs: Vec<TypeRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TypeRef {
    path: Vec<String>,
    name: String,
}

struct TypeIndex {
    by_key: BTreeMap<String, SerdeTypeDef>,
    by_name: BTreeMap<String, BTreeSet<String>>,
    return_refs_by_name: BTreeMap<String, Vec<TypeRef>>,
}

impl TypeIndex {
    fn new(sources: &[SourceFile]) -> Self {
        let mut by_key = BTreeMap::new();
        for source in sources {
            SerdeTypeDefVisitor {
                path_str: &source.path_str,
                by_key: &mut by_key,
            }
            .visit_file(&source.syntax);
        }

        let mut by_name = BTreeMap::<String, BTreeSet<String>>::new();
        for key in by_key.keys() {
            if let Some(name) = key.rsplit("::").next() {
                by_name
                    .entry(name.to_string())
                    .or_default()
                    .insert(key.clone());
            }
        }

        let mut returns = BTreeMap::<String, Option<Vec<TypeRef>>>::new();
        for source in sources {
            ReturnRefVisitor {
                return_refs_by_name: &mut returns,
            }
            .visit_file(&source.syntax);
        }
        let return_refs_by_name = returns
            .into_iter()
            .filter_map(|(name, refs)| refs.map(|refs| (name, refs)))
            .collect();

        Self {
            by_key,
            by_name,
            return_refs_by_name,
        }
    }

    fn resolve(
        &self,
        type_ref: &TypeRef,
        current_path: &str,
        imports: &BTreeMap<String, String>,
    ) -> Option<String> {
        if type_ref.path.is_empty() || is_external_path(&type_ref.path) {
            return None;
        }
        if type_ref.path.len() == 1 {
            let local_key = format!("{current_path}::{}", type_ref.name);
            if self.by_key.contains_key(&local_key) {
                return Some(local_key);
            }
            if let Some(imported_key) = imports.get(&type_ref.name) {
                return Some(imported_key.clone());
            }
            return self.unique_name_key(&type_ref.name);
        }
        if type_ref
            .path
            .first()
            .is_some_and(|segment| segment == "crate")
        {
            return self.resolve_crate_path(&type_ref.path);
        }
        if let Some(relative_key) = self.resolve_relative_path(current_path, &type_ref.path) {
            return Some(relative_key);
        }
        self.unique_name_key(&type_ref.name)
    }

    fn unique_name_key(&self, name: &str) -> Option<String> {
        let keys = self.by_name.get(name)?;
        (keys.len() == 1).then(|| keys.iter().next().unwrap().clone())
    }

    fn resolve_crate_path(&self, path: &[String]) -> Option<String> {
        let module_segments = path.get(1..path.len().saturating_sub(1))?;
        let name = path.last()?;
        module_file_candidates("src", module_segments)
            .into_iter()
            .map(|file_path| format!("{file_path}::{name}"))
            .find(|key| self.by_key.contains_key(key))
    }

    fn resolve_relative_path(&self, current_path: &str, path: &[String]) -> Option<String> {
        let module_segments = path.get(..path.len().saturating_sub(1))?;
        let name = path.last()?;
        let current_module_root = current_module_root(current_path)?;
        module_file_candidates(&current_module_root, module_segments)
            .into_iter()
            .map(|file_path| format!("{file_path}::{name}"))
            .find(|key| self.by_key.contains_key(key))
    }
}

struct SerdeTypeDefVisitor<'a> {
    path_str: &'a str,
    by_key: &'a mut BTreeMap<String, SerdeTypeDef>,
}

impl Visit<'_> for SerdeTypeDefVisitor<'_> {
    fn visit_item_struct(&mut self, item: &syn::ItemStruct) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        if has_serde_derive(&item.attrs) {
            let name = item.ident.to_string();
            let key = format!("{}::{name}", self.path_str);
            self.by_key.insert(
                key,
                SerdeTypeDef {
                    path_str: self.path_str.to_string(),
                    field_refs: field_type_refs(&item.fields),
                },
            );
        }
    }

    fn visit_item_enum(&mut self, item: &syn::ItemEnum) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        if has_serde_derive(&item.attrs) {
            let name = item.ident.to_string();
            let key = format!("{}::{name}", self.path_str);
            self.by_key.insert(
                key,
                SerdeTypeDef {
                    path_str: self.path_str.to_string(),
                    field_refs: item
                        .variants
                        .iter()
                        .flat_map(|variant| field_type_refs(&variant.fields))
                        .collect(),
                },
            );
        }
    }

    fn visit_item_mod(&mut self, item: &syn::ItemMod) {
        if !is_cfg_test(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }
}

struct ReturnRefVisitor<'a> {
    return_refs_by_name: &'a mut BTreeMap<String, Option<Vec<TypeRef>>>,
}

impl Visit<'_> for ReturnRefVisitor<'_> {
    fn visit_item_fn(&mut self, item: &syn::ItemFn) {
        if !is_cfg_test(&item.attrs) {
            record_return_refs(
                item.sig.ident.to_string(),
                &item.sig.output,
                self.return_refs_by_name,
            );
        }
    }

    fn visit_impl_item_fn(&mut self, item: &syn::ImplItemFn) {
        if !is_cfg_test(&item.attrs) {
            record_return_refs(
                item.sig.ident.to_string(),
                &item.sig.output,
                self.return_refs_by_name,
            );
        }
    }

    fn visit_item_mod(&mut self, item: &syn::ItemMod) {
        if !is_cfg_test(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }
}

struct RestUseSiteVisitor<'a> {
    source: &'a SourceFile,
    imports: &'a BTreeMap<String, String>,
    type_index: &'a TypeIndex,
    local_types: BTreeMap<String, Vec<TypeRef>>,
    raw_body_bindings: BTreeSet<String>,
    discovered: &'a mut BTreeSet<String>,
}

impl Visit<'_> for RestUseSiteVisitor<'_> {
    fn visit_item_fn(&mut self, item: &syn::ItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let previous_local_types = std::mem::take(&mut self.local_types);
        let previous_raw_body_bindings = std::mem::take(&mut self.raw_body_bindings);
        self.visit_signature(&item.sig);
        self.visit_block(&item.block);
        self.local_types = previous_local_types;
        self.raw_body_bindings = previous_raw_body_bindings;
    }

    fn visit_impl_item_fn(&mut self, item: &syn::ImplItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let previous_local_types = std::mem::take(&mut self.local_types);
        let previous_raw_body_bindings = std::mem::take(&mut self.raw_body_bindings);
        self.visit_signature(&item.sig);
        self.visit_block(&item.block);
        self.local_types = previous_local_types;
        self.raw_body_bindings = previous_raw_body_bindings;
    }

    fn visit_signature(&mut self, signature: &syn::Signature) {
        for input in &signature.inputs {
            if let syn::FnArg::Typed(input) = input {
                let refs = collect_rest_wrapper_type_refs(&input.ty);
                self.add_refs(&refs);
                for ident in pat_binding_idents(&input.pat) {
                    self.local_types.insert(ident, refs.clone());
                }
                if is_raw_body_type(&input.ty) {
                    self.raw_body_bindings
                        .extend(pat_binding_idents(&input.pat));
                }
            }
        }
        if let syn::ReturnType::Type(_, ty) = &signature.output {
            self.add_refs(&collect_rest_wrapper_type_refs(ty));
        }
    }

    fn visit_local(&mut self, local: &syn::Local) {
        let annotated_refs = pat_type_refs(&local.pat);
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if expr_contains_raw_body_from_slice_call(&init.expr, &self.raw_body_bindings) {
                if let Some(refs) = &annotated_refs {
                    self.add_refs(refs);
                }
            }
            let refs = infer_expr_type_refs(&init.expr, &self.local_types, self.type_index);
            if !refs.is_empty() {
                for ident in pat_binding_idents(&local.pat) {
                    self.local_types.insert(ident, refs.clone());
                }
            }
        }
        if let Some(refs) = annotated_refs {
            for ident in pat_binding_idents(&local.pat) {
                self.local_types.insert(ident, refs.clone());
            }
        }
    }

    fn visit_expr_call(&mut self, call: &syn::ExprCall) {
        if expr_path_last_ident(&call.func).as_deref() == Some("Json") {
            for arg in &call.args {
                self.collect_json_payload_expr_refs(arg);
            }
        }
        self.add_refs(&collect_raw_body_from_slice_type_refs(
            call,
            &self.raw_body_bindings,
        ));
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_match(&mut self, expr: &syn::ExprMatch) {
        self.visit_expr(&expr.expr);
        let matched_refs = infer_expr_type_refs(&expr.expr, &self.local_types, self.type_index);
        let previous = self.local_types.clone();
        for arm in &expr.arms {
            self.local_types = previous.clone();
            if !matched_refs.is_empty() {
                for ident in pat_binding_idents(&arm.pat) {
                    self.local_types.insert(ident, matched_refs.clone());
                }
            }
            if let Some((_, guard)) = &arm.guard {
                self.visit_expr(guard);
            }
            self.visit_expr(&arm.body);
        }
        self.local_types = previous;
    }

    fn visit_item_mod(&mut self, item: &syn::ItemMod) {
        if !is_cfg_test(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }
}

impl RestUseSiteVisitor<'_> {
    fn add_refs(&mut self, refs: &[TypeRef]) {
        for type_ref in refs {
            if let Some(key) =
                self.type_index
                    .resolve(type_ref, &self.source.path_str, self.imports)
            {
                self.discovered.insert(key);
            }
        }
    }

    fn collect_json_payload_expr_refs(&mut self, expr: &syn::Expr) {
        let refs = infer_expr_type_refs(expr, &self.local_types, self.type_index);
        self.add_refs(&refs);
        RestPayloadVisitor { parent: self }.visit_expr(expr);
    }

    fn collect_macro_local_refs(&mut self, tokens: &str) {
        let mut matched_refs = Vec::new();
        for token in tokens.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
            if let Some(refs) = self.local_types.get(token) {
                matched_refs.extend(refs.iter().cloned());
            } else if token.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                matched_refs.push(TypeRef {
                    path: vec![token.to_string()],
                    name: token.to_string(),
                });
            }
        }
        self.add_refs(&matched_refs);
    }
}

struct RestPayloadVisitor<'a, 'b> {
    parent: &'b mut RestUseSiteVisitor<'a>,
}

impl Visit<'_> for RestPayloadVisitor<'_, '_> {
    fn visit_expr_call(&mut self, call: &syn::ExprCall) {
        if is_serde_to_value_call(&call.func) {
            for arg in &call.args {
                let refs =
                    infer_expr_type_refs(arg, &self.parent.local_types, self.parent.type_index);
                self.parent.add_refs(&refs);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_macro(&mut self, expr: &syn::ExprMacro) {
        if is_json_macro_path(&expr.mac.path) {
            self.parent
                .collect_macro_local_refs(&expr.mac.tokens.to_string());
        }
    }

    fn visit_expr_path(&mut self, expr: &syn::ExprPath) {
        let refs = infer_expr_type_refs(
            &syn::Expr::Path(expr.clone()),
            &self.parent.local_types,
            self.parent.type_index,
        );
        self.parent.add_refs(&refs);
    }

    fn visit_expr_struct(&mut self, expr: &syn::ExprStruct) {
        if let Some(type_ref) = type_ref_from_path(&expr.path) {
            self.parent.add_refs(&[type_ref]);
        }
        visit::visit_expr_struct(self, expr);
    }
}

fn load_sources(manifest_dir: &Path) -> Vec<SourceFile> {
    let mut paths = Vec::new();
    collect_rust_source_paths(&manifest_dir.join("src"), &mut paths);
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let relative_path = path.strip_prefix(manifest_dir).unwrap_or(&path);
            let path_str = relative_path.to_string_lossy().replace('\\', "/");
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {path_str}: {error}"));
            let syntax = syn::parse_file(&source)
                .unwrap_or_else(|error| panic!("parse {path_str}: {error}"));
            SourceFile { path_str, syntax }
        })
        .collect()
}

fn collect_rust_source_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let entry =
            entry.unwrap_or_else(|error| panic!("read entry in {}: {error}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            collect_rust_source_paths(&path, paths);
        } else if is_production_rust_file(&path) {
            paths.push(path);
        }
    }
}

fn collect_imports(syntax: &syn::File, type_index: &TypeIndex) -> BTreeMap<String, String> {
    let mut imports = BTreeMap::new();
    for item in &syntax.items {
        if let syn::Item::Use(item_use) = item {
            collect_use_tree(Vec::new(), &item_use.tree, type_index, &mut imports);
        }
    }
    imports
}

fn collect_use_tree(
    mut prefix: Vec<String>,
    tree: &syn::UseTree,
    type_index: &TypeIndex,
    imports: &mut BTreeMap<String, String>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_tree(prefix, &path.tree, type_index, imports);
        }
        syn::UseTree::Name(name) => {
            let visible = name.ident.to_string();
            prefix.push(visible.clone());
            record_import(visible, prefix, type_index, imports);
        }
        syn::UseTree::Rename(rename) => {
            let visible = rename.rename.to_string();
            prefix.push(rename.ident.to_string());
            record_import(visible, prefix, type_index, imports);
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree(prefix.clone(), item, type_index, imports);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}

fn record_import(
    visible: String,
    path: Vec<String>,
    type_index: &TypeIndex,
    imports: &mut BTreeMap<String, String>,
) {
    if path.first().is_some_and(|segment| segment == "crate") {
        if let Some(key) = type_index.resolve_crate_path(&path) {
            imports.insert(visible, key);
        }
    }
}

fn record_return_refs(
    name: String,
    output: &syn::ReturnType,
    return_refs_by_name: &mut BTreeMap<String, Option<Vec<TypeRef>>>,
) {
    let syn::ReturnType::Type(_, ty) = output else {
        return;
    };
    let refs = collect_type_refs(ty);
    if refs.is_empty() {
        return;
    }
    match return_refs_by_name.entry(name) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(Some(refs));
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            if entry.get().as_ref() != Some(&refs) {
                entry.insert(None);
            }
        }
    }
}

fn infer_expr_type_refs(
    expr: &syn::Expr,
    local_types: &BTreeMap<String, Vec<TypeRef>>,
    type_index: &TypeIndex,
) -> Vec<TypeRef> {
    match expr {
        syn::Expr::Path(expr_path) if expr_path.path.segments.len() == 1 => {
            let name = expr_path.path.segments[0].ident.to_string();
            local_types.get(&name).cloned().unwrap_or_default()
        }
        syn::Expr::Path(expr_path) => type_ref_from_path(&expr_path.path).into_iter().collect(),
        syn::Expr::Struct(expr_struct) => {
            type_ref_from_path(&expr_struct.path).into_iter().collect()
        }
        syn::Expr::Reference(expr_reference) => {
            infer_expr_type_refs(&expr_reference.expr, local_types, type_index)
        }
        syn::Expr::Paren(expr_paren) => {
            infer_expr_type_refs(&expr_paren.expr, local_types, type_index)
        }
        syn::Expr::Group(expr_group) => {
            infer_expr_type_refs(&expr_group.expr, local_types, type_index)
        }
        syn::Expr::Try(expr_try) => infer_expr_type_refs(&expr_try.expr, local_types, type_index),
        syn::Expr::Await(expr_await) => {
            infer_expr_type_refs(&expr_await.base, local_types, type_index)
        }
        syn::Expr::Call(call) => expr_path_last_ident(&call.func)
            .and_then(|name| type_index.return_refs_by_name.get(&name).cloned())
            .unwrap_or_default(),
        syn::Expr::MethodCall(method_call) => {
            let method_name = method_call.method.to_string();
            if matches!(
                method_name.as_str(),
                "expect" | "unwrap" | "unwrap_or" | "unwrap_or_else"
            ) {
                let receiver_refs =
                    infer_expr_type_refs(&method_call.receiver, local_types, type_index);
                if !receiver_refs.is_empty() {
                    return receiver_refs;
                }
            }
            type_index
                .return_refs_by_name
                .get(&method_name)
                .cloned()
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn expand_transitive_rest_dtos(
    direct_use_sites: BTreeSet<String>,
    type_index: &TypeIndex,
) -> BTreeSet<String> {
    let mut discovered = BTreeSet::new();
    let mut stack = direct_use_sites.into_iter().collect::<Vec<_>>();

    while let Some(key) = stack.pop() {
        if !discovered.insert(key.clone()) {
            continue;
        }
        let Some(type_def) = type_index.by_key.get(&key) else {
            continue;
        };
        for field_ref in &type_def.field_refs {
            if let Some(field_key) =
                type_index.resolve(field_ref, &type_def.path_str, &BTreeMap::new())
            {
                stack.push(field_key);
            }
        }
    }

    discovered
}

fn collect_rest_wrapper_type_refs(ty: &syn::Type) -> Vec<TypeRef> {
    let mut visitor = RestWrapperTypeVisitor { refs: Vec::new() };
    visitor.visit_type(ty);
    visitor.refs
}

struct RestWrapperTypeVisitor {
    refs: Vec<TypeRef>,
}

impl Visit<'_> for RestWrapperTypeVisitor {
    fn visit_type_path(&mut self, type_path: &syn::TypePath) {
        for segment in &type_path.path.segments {
            if matches!(
                segment.ident.to_string().as_str(),
                "Form" | "Json" | "Path" | "Query"
            ) {
                self.refs
                    .extend(collect_type_refs_from_arguments(&segment.arguments));
            }
        }
        visit::visit_type_path(self, type_path);
    }
}

fn collect_type_refs(ty: &syn::Type) -> Vec<TypeRef> {
    let mut visitor = TypeRefVisitor { refs: Vec::new() };
    visitor.visit_type(ty);
    visitor.refs
}

struct TypeRefVisitor {
    refs: Vec<TypeRef>,
}

impl Visit<'_> for TypeRefVisitor {
    fn visit_type_path(&mut self, type_path: &syn::TypePath) {
        if let Some(type_ref) = type_ref_from_path(&type_path.path) {
            self.refs.push(type_ref);
        }
        visit::visit_type_path(self, type_path);
    }
}

fn collect_type_refs_from_arguments(arguments: &syn::PathArguments) -> Vec<TypeRef> {
    let mut refs = Vec::new();
    if let syn::PathArguments::AngleBracketed(args) = arguments {
        for arg in &args.args {
            if let syn::GenericArgument::Type(ty) = arg {
                refs.extend(collect_type_refs(ty));
            }
        }
    }
    refs
}
