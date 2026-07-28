//! Narrow accidental-drift guards for five fixed `harness-cli` entry points.
//!
//! This is not a Rust name resolver or security boundary. HIR-based Clippy
//! handles low-level constructors; the one review exception is scanned here.

use std::{
    fs,
    path::{Path, PathBuf},
};
use syn::{
    visit::{self, Visit},
    Attribute, Expr, ExprCall, ExprPath, File, Item, ItemFn, Local, Member, Pat, Stmt,
};

const REGISTRY_BUILDER: &str = "harness_agents::builder::registry_from_config";
const CLAUDE_BUILDER: &str = "harness_agents::builder::claude_agent_from_config";

#[derive(Clone, Copy)]
enum ExpectedUse {
    LetBinding(&'static str),
    TailExpression,
}

#[derive(Clone, Copy)]
struct EntryContract {
    relative_path: &'static str,
    function: &'static str,
    builder: &'static str,
    expected_use: ExpectedUse,
}

const ENTRY_CONTRACTS: [EntryContract; 5] = [
    EntryContract {
        relative_path: "src/commands/serve.rs",
        function: "run",
        builder: REGISTRY_BUILDER,
        expected_use: ExpectedUse::LetBinding("agent_registry"),
    },
    EntryContract {
        relative_path: "src/commands/exec.rs",
        function: "run",
        builder: REGISTRY_BUILDER,
        expected_use: ExpectedUse::LetBinding("agent_registry"),
    },
    EntryContract {
        relative_path: "src/gc.rs",
        function: "build_agent_registry",
        builder: REGISTRY_BUILDER,
        expected_use: ExpectedUse::TailExpression,
    },
    EntryContract {
        relative_path: "src/cmd/mcp_server.rs",
        function: "run",
        builder: REGISTRY_BUILDER,
        expected_use: ExpectedUse::LetBinding("agent_registry"),
    },
    EntryContract {
        relative_path: "src/cmd/pr.rs",
        function: "create_agent",
        builder: CLAUDE_BUILDER,
        expected_use: ExpectedUse::TailExpression,
    },
];

#[test]
fn fixed_entry_points_use_the_canonical_builder_directly() {
    for contract in ENTRY_CONTRACTS {
        let source = read_cli_source(contract.relative_path);
        verify_entry_contract(&source, contract).unwrap_or_else(|error| {
            panic!(
                "{}::{} violates its configured-agent contract: {error}",
                contract.relative_path, contract.function
            )
        });
    }
}

#[test]
fn codex_constructor_is_only_the_exact_read_only_review_exception() {
    let mut occurrences = Vec::new();
    let mut pr_source = None;

    for (relative_path, source) in production_sources() {
        let file = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("{} should parse: {error}", relative_path.display()));
        let mut visitor = CodexConstructorPathCounter::default();
        visitor.visit_file(&file);
        occurrences.extend(std::iter::repeat_n(
            relative_path.display().to_string(),
            visitor.count,
        ));
        if relative_path == Path::new("src/cmd/pr.rs") {
            pr_source = Some(source);
        }
    }

    assert_eq!(
        occurrences,
        ["src/cmd/pr.rs"],
        "all production `CodexAgent::new` references must be the single review-only exception"
    );
    verify_codex_review_exception(
        pr_source
            .as_deref()
            .expect("src/cmd/pr.rs should be included in the production scan"),
    )
    .unwrap_or_else(|error| panic!("cmd/pr.rs::review exception changed: {error}"));
}

fn verify_entry_contract(source: &str, contract: EntryContract) -> Result<(), String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let function = unique_top_level_function(&file, contract.function)?;
    reject_gated_attributes(&function.attrs, "function")?;

    let mut calls = ExactCallCounter::new(contract.builder);
    calls.visit_item_fn(function);
    if calls.count != 1 {
        return Err(format!(
            "expected exactly one direct `{}` call, found {}",
            contract.builder, calls.count
        ));
    }

    match contract.expected_use {
        ExpectedUse::LetBinding(binding) => verify_let_binding(function, contract.builder, binding),
        ExpectedUse::TailExpression => verify_tail_expression(function, contract.builder),
    }
}

fn verify_let_binding(function: &ItemFn, builder: &str, binding: &str) -> Result<(), String> {
    let candidates = function
        .block
        .stmts
        .iter()
        .enumerate()
        .filter_map(|(index, statement)| {
            let Stmt::Local(local) = statement else {
                return None;
            };
            let init = local.init.as_ref()?;
            let (call, _) = transparent_root_call(&init.expr)?;
            (binding_matches(&local.pat, binding) && call_matches(call, builder))
                .then_some((index, local))
        })
        .collect::<Vec<_>>();

    let [(binding_index, local)] = candidates.as_slice() else {
        return Err(format!(
            "expected one top-level `{binding}` binding initialized by `{builder}`, found {}",
            candidates.len()
        ));
    };
    if local
        .init
        .as_ref()
        .is_some_and(|init| init.diverge.is_some())
    {
        return Err(format!("`{binding}` must not use a let-else initializer"));
    }
    reject_gated_attributes(&local.attrs, "builder binding")?;
    let (_, gated) = transparent_root_call(
        &local
            .init
            .as_ref()
            .expect("candidate has an initializer")
            .expr,
    )
    .expect("candidate has a transparent root call");
    if gated {
        return Err("builder binding must be unconditional".to_string());
    }
    if function.block.stmts[..*binding_index]
        .iter()
        .any(is_top_level_return)
    {
        return Err("an unconditional return makes the builder binding unreachable".to_string());
    }

    let later_statements = &function.block.stmts[*binding_index + 1..];
    let mut shadow_counter = BindingCounter::new(binding);
    for statement in later_statements {
        shadow_counter.visit_stmt(statement);
    }
    if shadow_counter.count != 0 {
        return Err(format!(
            "`{binding}` is shadowed after the canonical builder binding"
        ));
    }

    let mut used_before_return = false;
    for statement in later_statements {
        let mut use_counter = PathUseCounter::new(binding);
        use_counter.visit_stmt(statement);
        used_before_return |= use_counter.count > 0;
        if is_top_level_return(statement) {
            break;
        }
    }
    if !used_before_return {
        return Err(format!(
            "`{binding}` must be used after construction on the reachable top-level path"
        ));
    }

    Ok(())
}

fn verify_tail_expression(function: &ItemFn, builder: &str) -> Result<(), String> {
    let Some((tail, preceding)) = function.block.stmts.split_last() else {
        return Err("function body is empty".to_string());
    };
    let Stmt::Expr(expression, None) = tail else {
        return Err(format!(
            "`{builder}` must be the function's tail expression"
        ));
    };
    let Some((call, gated)) = transparent_root_call(expression) else {
        return Err(format!(
            "`{builder}` must be called directly as the tail expression"
        ));
    };
    if !call_matches(call, builder) {
        return Err(format!("tail expression must call `{builder}`"));
    }
    if gated {
        return Err("tail builder call must be unconditional".to_string());
    }
    if preceding.iter().any(is_top_level_return) {
        return Err("an unconditional return makes the tail builder call unreachable".to_string());
    }
    Ok(())
}

fn unique_top_level_function<'a>(file: &'a File, name: &str) -> Result<&'a ItemFn, String> {
    let matches = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) if function.sig.ident == name => Some(function),
            _ => None,
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [function] => Ok(function),
        _ => Err(format!(
            "expected exactly one top-level `{name}` function, found {}",
            matches.len()
        )),
    }
}

fn transparent_root_call(expression: &Expr) -> Option<(&ExprCall, bool)> {
    let mut current = expression;
    let mut gated = false;
    loop {
        match current {
            Expr::Try(node) => {
                gated |= has_gate(&node.attrs);
                current = &node.expr;
            }
            Expr::Paren(node) => {
                gated |= has_gate(&node.attrs);
                current = &node.expr;
            }
            Expr::Group(node) => {
                gated |= has_gate(&node.attrs);
                current = &node.expr;
            }
            Expr::Call(call) => {
                gated |= has_gate(&call.attrs);
                if let Expr::Path(path) = call.func.as_ref() {
                    gated |= has_gate(&path.attrs);
                }
                return Some((call, gated));
            }
            _ => return None,
        }
    }
}

fn call_matches(call: &ExprCall, expected: &str) -> bool {
    matches!(
        call.func.as_ref(),
        Expr::Path(path) if path.qself.is_none() && path_matches(&path.path, expected)
    )
}

fn path_matches(path: &syn::Path, expected: &str) -> bool {
    path.leading_colon.is_none()
        && path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .eq(expected.split("::").map(str::to_string))
}

fn path_ends_with(path: &syn::Path, suffix: &[&str]) -> bool {
    let segments = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    segments.len() >= suffix.len()
        && segments[segments.len() - suffix.len()..]
            .iter()
            .map(String::as_str)
            .eq(suffix.iter().copied())
}

fn binding_matches(pattern: &Pat, expected: &str) -> bool {
    let Pat::Ident(binding) = pattern else {
        return false;
    };
    binding.by_ref.is_none() && binding.subpat.is_none() && binding.ident == expected
}

fn has_gate(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.path().segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "test" | "cfg" | "cfg_attr"
            )
        })
    })
}

fn reject_gated_attributes(attributes: &[Attribute], subject: &str) -> Result<(), String> {
    if has_gate(attributes) {
        Err(format!(
            "{subject} must not have `test`, `cfg`, or `cfg_attr` gating"
        ))
    } else {
        Ok(())
    }
}

fn is_top_level_return(statement: &Stmt) -> bool {
    matches!(statement, Stmt::Expr(Expr::Return(_), _))
}

struct ExactCallCounter<'a> {
    expected: &'a str,
    count: usize,
}

impl<'a> ExactCallCounter<'a> {
    fn new(expected: &'a str) -> Self {
        Self { expected, count: 0 }
    }
}

impl<'ast> Visit<'ast> for ExactCallCounter<'_> {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if call_matches(call, self.expected) {
            self.count += 1;
        }
        visit::visit_expr_call(self, call);
    }
}

struct BindingCounter<'a> {
    expected: &'a str,
    count: usize,
}

impl<'a> BindingCounter<'a> {
    fn new(expected: &'a str) -> Self {
        Self { expected, count: 0 }
    }
}

impl<'ast> Visit<'ast> for BindingCounter<'_> {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        if pattern.ident == self.expected {
            self.count += 1;
        }
        visit::visit_pat_ident(self, pattern);
    }
}

struct PathUseCounter<'a> {
    expected: &'a str,
    count: usize,
}

impl<'a> PathUseCounter<'a> {
    fn new(expected: &'a str) -> Self {
        Self { expected, count: 0 }
    }
}

impl<'ast> Visit<'ast> for PathUseCounter<'_> {
    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        if path.qself.is_none() && path_matches(&path.path, self.expected) {
            self.count += 1;
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_expr_async(&mut self, _expression: &'ast syn::ExprAsync) {}

    fn visit_expr_closure(&mut self, _expression: &'ast syn::ExprClosure) {}

    fn visit_expr_const(&mut self, _expression: &'ast syn::ExprConst) {}

    fn visit_item(&mut self, _item: &'ast Item) {}
}

#[derive(Default)]
struct CodexConstructorPathCounter {
    count: usize,
}

impl<'ast> Visit<'ast> for CodexConstructorPathCounter {
    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        if path.qself.is_none() && path_ends_with(&path.path, &["CodexAgent", "new"]) {
            self.count += 1;
        }
        visit::visit_expr_path(self, path);
    }
}

fn verify_codex_review_exception(source: &str) -> Result<(), String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let review = unique_top_level_function(&file, "review")?;
    reject_gated_attributes(&review.attrs, "review function")?;

    let matches = review
        .block
        .stmts
        .iter()
        .filter_map(|statement| {
            let Stmt::Local(local) = statement else {
                return None;
            };
            let Pat::Ident(pattern) = &local.pat else {
                return None;
            };
            (pattern.ident == "agent"
                && pattern.mutability.is_some()
                && pattern.by_ref.is_none()
                && pattern.subpat.is_none())
            .then_some(local)
        })
        .filter(|local| codex_review_initializer(local).is_ok())
        .count();

    if matches == 1 {
        Ok(())
    } else {
        Err(format!(
            "expected one exact read-only Codex review initializer, found {matches}"
        ))
    }
}

fn codex_review_initializer(local: &Local) -> Result<(), String> {
    reject_gated_attributes(&local.attrs, "Codex review binding")?;
    let init = local
        .init
        .as_ref()
        .ok_or_else(|| "Codex review binding needs an initializer".to_string())?;
    if init.diverge.is_some() {
        return Err("Codex review binding must not use let-else".to_string());
    }
    let Expr::MethodCall(timeout) = init.expr.as_ref() else {
        return Err("Codex review initializer must set its stream timeout".to_string());
    };
    if timeout.method != "with_stream_timeout"
        || timeout.turbofish.is_some()
        || timeout.args.len() != 1
        || has_gate(&timeout.attrs)
    {
        return Err("Codex review timeout chain changed".to_string());
    }
    let Expr::Call(constructor) = timeout.receiver.as_ref() else {
        return Err("Codex review timeout must chain directly from construction".to_string());
    };
    if !call_matches(constructor, "CodexAgent::new")
        || constructor.args.len() != 2
        || has_gate(&constructor.attrs)
    {
        return Err("Codex review must call `CodexAgent::new` directly".to_string());
    }
    let mut args = constructor.args.iter();
    if !is_review_config_clone(
        args.next().expect("constructor has two arguments"),
        "cli_path",
    ) {
        return Err("Codex review must use `review_config.cli_path.clone()`".to_string());
    }
    if !is_exact_expr_path(
        args.next().expect("constructor has two arguments"),
        "SandboxMode::ReadOnlyWithNetwork",
    ) {
        return Err("Codex review sandbox must remain read-only with network".to_string());
    }
    if !is_some_review_config_field(
        timeout.args.first().expect("timeout has one argument"),
        "timeout_secs",
    ) {
        return Err("Codex review must use its configured timeout".to_string());
    }
    Ok(())
}

fn is_review_config_clone(expression: &Expr, field: &str) -> bool {
    let Expr::MethodCall(clone) = expression else {
        return false;
    };
    clone.method == "clone"
        && clone.turbofish.is_none()
        && clone.args.is_empty()
        && is_review_config_field(&clone.receiver, field)
}

fn is_some_review_config_field(expression: &Expr, field: &str) -> bool {
    let Expr::Call(call) = expression else {
        return false;
    };
    call.args.len() == 1
        && call_matches(call, "Some")
        && call
            .args
            .first()
            .is_some_and(|argument| is_review_config_field(argument, field))
}

fn is_review_config_field(expression: &Expr, field: &str) -> bool {
    let Expr::Field(field_expression) = expression else {
        return false;
    };
    matches!(&field_expression.member, Member::Named(name) if name == field)
        && is_exact_expr_path(&field_expression.base, "review_config")
}

fn is_exact_expr_path(expression: &Expr, expected: &str) -> bool {
    matches!(
        expression,
        Expr::Path(path) if path.qself.is_none() && path_matches(&path.path, expected)
    )
}

fn read_cli_source(relative_path: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn production_sources() -> Vec<(PathBuf, String)> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    collect_rust_sources(&manifest_dir.join("src"), &mut paths)
        .unwrap_or_else(|error| panic!("failed to scan harness-cli sources: {error}"));
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(manifest_dir)
                .expect("source should be below the crate root")
                .to_path_buf();
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            (relative, source)
        })
        .collect()
}

fn collect_rust_sources(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_rust_sources(&path, paths)?;
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        {
            paths.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod regressions {
    use super::*;

    const LET_CONTRACT: EntryContract = EntryContract {
        relative_path: "fixture.rs",
        function: "run",
        builder: REGISTRY_BUILDER,
        expected_use: ExpectedUse::LetBinding("agent_registry"),
    };
    const TAIL_CONTRACT: EntryContract = EntryContract {
        relative_path: "fixture.rs",
        function: "create_agent",
        builder: CLAUDE_BUILDER,
        expected_use: ExpectedUse::TailExpression,
    };

    #[test]
    fn direct_ungated_binding_with_later_use_passes() {
        expect_pass(
            r#"
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume(&agent_registry);
            }
            "#,
            LET_CONTRACT,
        );
    }

    #[test]
    fn function_and_statement_test_gates_fail_closed() {
        for source in [
            gated_function("#[test]"),
            gated_function("#[cfg(unix)]"),
            gated_function("#[cfg_attr(test, test)]"),
            r#"
            fn run() {
                #[cfg(unix)]
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume(agent_registry);
            }
            "#
            .to_string(),
        ] {
            expect_reject(&source, LET_CONTRACT);
        }
    }

    #[test]
    fn nested_const_closure_and_local_function_bait_do_not_pass() {
        for source in [
            r#"
            fn run() {
                {
                    let agent_registry =
                        harness_agents::builder::registry_from_config()?;
                    consume(agent_registry);
                }
            }
            "#,
            r#"
            fn run() {
                let _bait = || harness_agents::builder::registry_from_config();
            }
            "#,
            r#"
            fn run() {
                const BAIT: () = {
                    harness_agents::builder::registry_from_config();
                };
            }
            "#,
            r#"
            fn run() {
                fn bait() {
                    harness_agents::builder::registry_from_config();
                }
            }
            "#,
        ] {
            expect_reject(source, LET_CONTRACT);
        }
    }

    #[test]
    fn unreachable_shadowed_and_unused_bindings_are_rejected() {
        for source in [
            r#"
            fn run() {
                return;
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume(agent_registry);
            }
            "#,
            r#"
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                let agent_registry = replacement();
                consume(agent_registry);
            }
            "#,
            r#"
            fn run() {
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume_something_else();
            }
            "#,
        ] {
            expect_reject(source, LET_CONTRACT);
        }
    }

    #[test]
    fn builder_alias_is_not_treated_as_the_canonical_call() {
        expect_reject(
            r#"
            use harness_agents::builder::registry_from_config as build_registry;
            fn run() {
                let agent_registry = build_registry()?;
                consume(agent_registry);
            }
            "#,
            LET_CONTRACT,
        );
    }

    #[test]
    fn direct_ungated_reachable_tail_call_passes() {
        expect_pass(
            r#"
            fn create_agent() {
                harness_agents::builder::claude_agent_from_config()
            }
            "#,
            TAIL_CONTRACT,
        );
    }

    #[test]
    fn tail_call_bait_and_unreachable_tail_are_rejected() {
        for source in [
            r#"
            fn create_agent() {
                || harness_agents::builder::claude_agent_from_config()
            }
            "#,
            r#"
            fn create_agent() {
                {
                    harness_agents::builder::claude_agent_from_config()
                }
            }
            "#,
            r#"
            fn create_agent() {
                return fallback();
                harness_agents::builder::claude_agent_from_config()
            }
            "#,
        ] {
            expect_reject(source, TAIL_CONTRACT);
        }
    }

    #[test]
    fn review_exception_rejects_a_non_read_only_sandbox() {
        let source = r#"
            fn review() {
                let mut agent = CodexAgent::new(
                    review_config.cli_path.clone(),
                    SandboxMode::DangerFullAccess,
                )
                .with_stream_timeout(Some(review_config.timeout_secs));
            }
        "#;
        assert!(verify_codex_review_exception(source).is_err());
    }

    fn gated_function(attribute: &str) -> String {
        format!(
            r#"
            {attribute}
            fn run() {{
                let agent_registry =
                    harness_agents::builder::registry_from_config()?;
                consume(agent_registry);
            }}
            "#
        )
    }

    fn expect_pass(source: &str, contract: EntryContract) {
        verify_entry_contract(source, contract)
            .unwrap_or_else(|error| panic!("fixture should pass: {error}"));
    }

    fn expect_reject(source: &str, contract: EntryContract) {
        assert!(
            verify_entry_contract(source, contract).is_err(),
            "fixture should be rejected"
        );
    }
}
