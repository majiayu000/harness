use std::{collections::BTreeSet, path::Path};
use syn::visit::{self, Visit};

use super::{collect_type_refs, collect_type_refs_from_arguments, TypeRef};

pub(super) fn field_type_refs(fields: &syn::Fields) -> Vec<TypeRef> {
    fields
        .iter()
        .filter(|field| !is_serde_skip(&field.attrs))
        .flat_map(|field| collect_type_refs(&field.ty))
        .collect()
}

pub(super) fn type_ref_from_path(path: &syn::Path) -> Option<TypeRef> {
    let path = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    let name = path.last()?.clone();
    Some(TypeRef { path, name })
}

pub(super) fn pat_type_refs(pat: &syn::Pat) -> Option<Vec<TypeRef>> {
    match pat {
        syn::Pat::Type(pat_type) => Some(collect_type_refs(&pat_type.ty)),
        _ => None,
    }
}

pub(super) fn pat_binding_idents(pat: &syn::Pat) -> Vec<String> {
    let mut visitor = PatBindingVisitor { idents: Vec::new() };
    visitor.visit_pat(pat);
    visitor.idents
}

struct PatBindingVisitor {
    idents: Vec<String>,
}

impl Visit<'_> for PatBindingVisitor {
    fn visit_pat_ident(&mut self, pat: &syn::PatIdent) {
        self.idents.push(pat.ident.to_string());
    }
}

pub(super) fn expr_path_last_ident(expr: &syn::Expr) -> Option<String> {
    let syn::Expr::Path(expr_path) = expr else {
        return None;
    };
    expr_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

pub(super) fn is_json_macro_path(path: &syn::Path) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == "json")
}

pub(super) fn is_serde_to_value_call(expr: &syn::Expr) -> bool {
    is_serde_json_call(expr, "to_value")
}

pub(super) fn collect_raw_body_from_slice_type_refs(
    call: &syn::ExprCall,
    raw_body_bindings: &BTreeSet<String>,
) -> Vec<TypeRef> {
    let Some(segment) = serde_json_call_segment(&call.func, "from_slice") else {
        return Vec::new();
    };
    if call
        .args
        .iter()
        .any(|arg| expr_references_binding(arg, raw_body_bindings))
    {
        collect_type_refs_from_arguments(&segment.arguments)
    } else {
        Vec::new()
    }
}

pub(super) fn expr_contains_raw_body_from_slice_call(
    expr: &syn::Expr,
    raw_body_bindings: &BTreeSet<String>,
) -> bool {
    let mut visitor = RawBodyFromSliceVisitor {
        raw_body_bindings,
        found: false,
    };
    visitor.visit_expr(expr);
    visitor.found
}

struct RawBodyFromSliceVisitor<'a> {
    raw_body_bindings: &'a BTreeSet<String>,
    found: bool,
}

impl Visit<'_> for RawBodyFromSliceVisitor<'_> {
    fn visit_expr_call(&mut self, call: &syn::ExprCall) {
        if is_serde_json_call(&call.func, "from_slice")
            && call
                .args
                .iter()
                .any(|arg| expr_references_binding(arg, self.raw_body_bindings))
        {
            self.found = true;
        }
        visit::visit_expr_call(self, call);
    }
}

fn expr_references_binding(expr: &syn::Expr, bindings: &BTreeSet<String>) -> bool {
    match expr {
        syn::Expr::Path(expr_path) if expr_path.path.segments.len() == 1 => {
            bindings.contains(&expr_path.path.segments[0].ident.to_string())
        }
        syn::Expr::Reference(expr) => expr_references_binding(&expr.expr, bindings),
        syn::Expr::MethodCall(expr) => expr_references_binding(&expr.receiver, bindings),
        syn::Expr::Paren(expr) => expr_references_binding(&expr.expr, bindings),
        syn::Expr::Group(expr) => expr_references_binding(&expr.expr, bindings),
        _ => false,
    }
}

pub(super) fn is_raw_body_type(ty: &syn::Type) -> bool {
    let syn::Type::Path(type_path) = ty else {
        return false;
    };
    type_path
        .path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Bytes")
}

fn is_serde_json_call(expr: &syn::Expr, name: &str) -> bool {
    serde_json_call_segment(expr, name).is_some()
}

fn serde_json_call_segment<'a>(expr: &'a syn::Expr, name: &str) -> Option<&'a syn::PathSegment> {
    let syn::Expr::Path(expr_path) = expr else {
        return None;
    };
    let segment = expr_path.path.segments.last()?;
    (segment.ident == name
        && expr_path
            .path
            .segments
            .iter()
            .any(|segment| segment.ident == "serde_json"))
    .then_some(segment)
}

pub(super) fn module_file_candidates(root: &str, module_segments: &[String]) -> Vec<String> {
    if module_segments.is_empty() {
        return vec![root.to_string()];
    }
    let module_path = module_segments.join("/");
    vec![
        format!("{root}/{module_path}.rs"),
        format!("{root}/{module_path}/mod.rs"),
    ]
}

pub(super) fn current_module_root(current_path: &str) -> Option<String> {
    if let Some(path) = current_path.strip_suffix("/mod.rs") {
        return Some(path.to_string());
    }
    current_path.strip_suffix(".rs").map(ToOwned::to_owned)
}

const EXTERNAL_PATH_ROOTS: &str = "anyhow axum chrono harness_agents harness_core harness_exec harness_gc harness_protocol harness_rules harness_skills harness_workflow serde_json sqlx std tokio uuid";

pub(super) fn is_external_path(path: &[String]) -> bool {
    path.first().is_some_and(|root| {
        EXTERNAL_PATH_ROOTS
            .split_whitespace()
            .any(|external| external == root)
    })
}

pub(super) fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attr_tokens_contain(attrs, "cfg", |token| token == "test")
}

fn is_serde_skip(attrs: &[syn::Attribute]) -> bool {
    attr_tokens_contain(attrs, "serde", |token| token == "skip")
}

pub(super) fn has_serde_derive(attrs: &[syn::Attribute]) -> bool {
    attr_tokens_contain(attrs, "derive", |token| {
        matches!(token, "Serialize" | "Deserialize")
    })
}

fn attr_tokens_contain(
    attrs: &[syn::Attribute],
    attr_name: &str,
    accepts: impl Fn(&str) -> bool,
) -> bool {
    attrs.iter().any(|attr| {
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        attr.path().is_ident(attr_name)
            && list
                .tokens
                .to_string()
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(&accepts)
    })
}

pub(super) fn is_production_rust_file(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.ends_with("_tests.rs") && !name.starts_with("tests_") && name != "test_fixtures.rs"
}
