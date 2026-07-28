use super::{
    cfg_test::{has_cfg_test, CfgTestModules},
    resolution::{
        alias_scope_from_block, alias_scope_from_items, collect_crate_aliases,
        collect_generic_factories, ident_name, path_segments, type_paths, AliasScope, CrateAliases,
        GenericFactories, Resolver, Segments,
    },
    BuilderCall, BuilderCallUse, DirectConstruction, ExpectedBuilderUse, MacroViolation,
    SourceUnit,
};
use proc_macro2::{TokenStream, TokenTree};
use std::{collections::HashMap, path::PathBuf};
use syn::{
    visit::{self, Visit},
    Block, Expr, ExprAsync, ExprCall, ExprClosure, ExprPath, ExprReturn, ExprStruct, File,
    ImplItemFn, ItemFn, ItemImpl, ItemMod, ItemUse, Local, Macro, Member, Pat, ReturnType, Stmt,
    Type, UseTree, Visibility,
};

const FORBIDDEN_TYPES: [&str; 7] = [
    "AgentRegistry",
    "ClaudeCodeAgent",
    "CodexAgent",
    "ClaudeAdapter",
    "CodexAdapter",
    "AnthropicApiAgent",
    "ProviderBackpressureGate",
];

#[derive(Default)]
pub(super) struct SourceAnalysis {
    direct_builder_calls: Vec<BuilderCall>,
    pub(super) direct_constructions: Vec<DirectConstruction>,
    pub(super) macro_violations: Vec<MacroViolation>,
    pub(super) production_glob_imports: usize,
}

impl SourceAnalysis {
    pub(super) fn direct_builder_call_count(&self, expected_path: &str) -> usize {
        let expected = expected_path.split("::").collect::<Vec<_>>();
        self.direct_builder_calls
            .iter()
            .filter(|call| {
                call.path
                    .iter()
                    .map(String::as_str)
                    .eq(expected.iter().copied())
            })
            .count()
    }

    pub(super) fn required_builder_call_count(
        &self,
        expected_path: &str,
        function: &str,
        expected_usage: ExpectedBuilderUse,
    ) -> usize {
        let expected = expected_path.split("::").collect::<Vec<_>>();
        self.direct_builder_calls
            .iter()
            .filter(|call| {
                call.path
                    .iter()
                    .map(String::as_str)
                    .eq(expected.iter().copied())
                    && call.function.as_deref() == Some(function)
                    && call.direct_function_body
                    && match (&call.usage, expected_usage) {
                        (
                            BuilderCallUse::LetBinding(actual),
                            ExpectedBuilderUse::LetBinding(expected),
                        ) => actual == expected,
                        (BuilderCallUse::TailExpression, ExpectedBuilderUse::TailExpression) => {
                            true
                        }
                        _ => false,
                    }
            })
            .count()
    }
}

struct FunctionFrame {
    name: String,
    body_address: usize,
    top_level_item: bool,
    return_forbidden_types: Vec<String>,
    return_scope_depth: usize,
}

struct SourceScanner<'a> {
    crate_aliases: &'a CrateAliases,
    generic_factories: &'a GenericFactories,
    scopes: Vec<AliasScope>,
    module_path: Segments,
    impl_types: Vec<Segments>,
    function_frames: Vec<FunctionFrame>,
    builder_usage_targets: Vec<(usize, BuilderCallUse)>,
    review_constructor_targets: Vec<usize>,
    inline_module_depth: usize,
    block_depth: usize,
    return_scope_depth: usize,
    in_cfg_test_module: bool,
    analysis: SourceAnalysis,
}

impl<'a> SourceScanner<'a> {
    fn new(
        crate_aliases: &'a CrateAliases,
        generic_factories: &'a GenericFactories,
        module_path: Segments,
        in_cfg_test_module: bool,
    ) -> Self {
        Self {
            crate_aliases,
            generic_factories,
            scopes: Vec::new(),
            module_path,
            impl_types: Vec::new(),
            function_frames: Vec::new(),
            builder_usage_targets: Vec::new(),
            review_constructor_targets: Vec::new(),
            inline_module_depth: 0,
            block_depth: 0,
            return_scope_depth: 0,
            in_cfg_test_module,
            analysis: SourceAnalysis::default(),
        }
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver::new(
            self.crate_aliases,
            &self.scopes,
            &self.module_path,
            &self.impl_types,
        )
    }

    fn enclosing_function(&self) -> Option<String> {
        self.function_frames.last().map(|frame| frame.name.clone())
    }

    fn in_top_level_item_function(&self) -> bool {
        self.function_frames.len() == 1
            && self
                .function_frames
                .last()
                .is_some_and(|frame| frame.top_level_item)
    }

    fn in_direct_function_body(&self, block: &Block) -> bool {
        self.function_frames.last().is_some_and(|frame| {
            frame.top_level_item
                && self.function_frames.len() == 1
                && frame.body_address == block as *const Block as usize
        })
    }

    fn builder_usage(&self, expression: &ExprCall) -> BuilderCallUse {
        let address = expression as *const ExprCall as usize;
        self.builder_usage_targets
            .iter()
            .rev()
            .find(|(target, _)| *target == address)
            .map_or(BuilderCallUse::Other, |(_, usage)| usage.clone())
    }

    fn forbidden_types_in_type(&self, type_: &Type) -> Vec<String> {
        let mut forbidden = Vec::new();
        for path in type_paths(type_) {
            for resolved in self.resolver().resolve(path) {
                let Some((type_name, _)) = forbidden_type_in_path(&resolved) else {
                    continue;
                };
                if !forbidden.contains(&type_name) {
                    forbidden.push(type_name);
                }
            }
        }
        forbidden
    }

    fn record_typed_construction(&mut self, type_names: &[String], expression: &Expr) {
        let Some(syntax) = trait_constructor_syntax(expression) else {
            return;
        };
        let module_path = self.module_path.join("::");
        let enclosing_function = self.enclosing_function();
        self.analysis
            .direct_constructions
            .extend(type_names.iter().map(|type_name| DirectConstruction {
                type_name: type_name.clone(),
                syntax: format!("{syntax} -> {type_name}"),
                module_path: module_path.clone(),
                enclosing_function: enclosing_function.clone(),
                intentional_pr_review_constructor: false,
            }));
    }

    fn record_generic_factory_call(&mut self, call: &ExprCall, function: &ExprPath) {
        if function.qself.is_some() || !call.args.is_empty() {
            return;
        }
        let original = path_segments(&function.path);
        let resolved = self.resolver().resolve(original.clone());
        let local_positions = if original.len() == 1 {
            self.scopes
                .iter()
                .rev()
                .find_map(|scope| scope.local_function_output_positions(original[0].as_str()))
        } else {
            None
        };
        let output_types = self.generic_factories.output_type_arguments(
            &function.path,
            &resolved,
            &self.module_path,
            local_positions,
        );
        let mut forbidden = Vec::new();
        for type_name in output_types
            .into_iter()
            .flat_map(|output_type| self.forbidden_types_in_type(output_type))
        {
            if !forbidden.contains(&type_name) {
                forbidden.push(type_name);
            }
        }
        let module_path = self.module_path.join("::");
        let enclosing_function = self.enclosing_function();
        self.analysis
            .direct_constructions
            .extend(forbidden.into_iter().map(|type_name| DirectConstruction {
                syntax: format!("{}::<{type_name}>", original.join("::")),
                type_name,
                module_path: module_path.clone(),
                enclosing_function: enclosing_function.clone(),
                intentional_pr_review_constructor: false,
            }));
    }

    fn record_expr_path(&mut self, expression: &ExprPath, call: Option<&ExprCall>) {
        let mut candidates = Vec::new();
        if let Some(qself) = &expression.qself {
            if let Type::Path(type_path) = qself.ty.as_ref() {
                for type_path in self.resolver().resolve(path_segments(&type_path.path)) {
                    let mut candidate = type_path;
                    candidate.extend(path_segments(&expression.path));
                    candidates.push(candidate);
                }
            }
        } else {
            candidates = self.resolver().resolve(path_segments(&expression.path));
        }

        let original = path_segments(&expression.path).join("::");
        let mut expression_constructions = Vec::new();
        for candidate in candidates {
            let Some((type_name, associated_item)) = forbidden_type_in_path(&candidate) else {
                continue;
            };
            let construction = DirectConstruction {
                intentional_pr_review_constructor: type_name == "CodexAgent"
                    && associated_item.as_deref() == Some("new")
                    && candidate
                        == ["harness_agents", "codex", "CodexAgent", "new"].map(str::to_string)
                    && original == "CodexAgent::new"
                    && call.is_some_and(intentional_codex_review_call)
                    && call.is_some_and(|call| {
                        let address = call as *const ExprCall as usize;
                        self.review_constructor_targets.contains(&address)
                    })
                    && self.in_top_level_item_function()
                    && self
                        .function_frames
                        .last()
                        .is_some_and(|frame| frame.name == "review"),
                type_name,
                syntax: original.clone(),
                module_path: self.module_path.join("::"),
                enclosing_function: self.enclosing_function(),
            };
            if !expression_constructions.contains(&construction) {
                expression_constructions.push(construction);
            }
        }
        self.analysis
            .direct_constructions
            .extend(expression_constructions);
    }

    fn scan_macro_tokens(&mut self, macro_: &Macro) {
        let mut paths = Vec::new();
        token_paths(macro_.tokens.clone(), &mut paths);
        for path in paths {
            if path.first().is_some_and(|segment| segment == "Self") {
                let violation = MacroViolation {
                    macro_path: path_segments(&macro_.path).join("::"),
                    forbidden_type: "unresolved Self".to_string(),
                };
                if !self.analysis.macro_violations.contains(&violation) {
                    self.analysis.macro_violations.push(violation);
                }
            }
            for resolved in self.resolver().resolve(path) {
                let Some((forbidden_type, _)) = forbidden_type_in_path(&resolved) else {
                    continue;
                };
                let violation = MacroViolation {
                    macro_path: path_segments(&macro_.path).join("::"),
                    forbidden_type,
                };
                if !self.analysis.macro_violations.contains(&violation) {
                    self.analysis.macro_violations.push(violation);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for SourceScanner<'_> {
    fn visit_file(&mut self, file: &'ast File) {
        self.scopes
            .push(alias_scope_from_items(&file.items, &self.module_path));
        for item in &file.items {
            self.visit_item(item);
        }
        self.scopes.pop();
    }

    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        let Some((_, items)) = &item.content else {
            return;
        };
        let parent_scopes = std::mem::take(&mut self.scopes);
        let parent_test_module = self.in_cfg_test_module;
        self.in_cfg_test_module |= has_cfg_test(&item.attrs);
        self.inline_module_depth += 1;
        self.module_path.push(ident_name(&item.ident));
        self.scopes
            .push(alias_scope_from_items(items, &self.module_path));
        for nested in items {
            self.visit_item(nested);
        }
        self.scopes.clear();
        self.module_path.pop();
        self.inline_module_depth -= 1;
        self.in_cfg_test_module = parent_test_module;
        self.scopes = parent_scopes;
    }

    fn visit_block(&mut self, block: &'ast Block) {
        self.block_depth += 1;
        self.scopes
            .push(alias_scope_from_block(block, &self.module_path));
        let function_body = self
            .function_frames
            .last()
            .filter(|frame| frame.body_address == block as *const Block as usize);
        let return_forbidden_types =
            function_body.map(|frame| frame.return_forbidden_types.clone());
        let direct_function_body = self.in_direct_function_body(block);
        let review_function =
            direct_function_body && function_body.is_some_and(|frame| frame.name == "review");

        if let (Some(type_names), Some(Stmt::Expr(expression, None))) =
            (return_forbidden_types, block.stmts.last())
        {
            self.record_typed_construction(&type_names, expression);
        }

        for (index, statement) in block.stmts.iter().enumerate() {
            let builder_target = direct_function_body
                .then(|| direct_builder_target(statement, index + 1 == block.stmts.len()))
                .flatten();
            let review_target = review_function
                .then(|| direct_review_constructor_target(statement))
                .flatten();
            if let Some(target) = builder_target.clone() {
                self.builder_usage_targets.push(target);
            }
            if let Some(target) = review_target {
                self.review_constructor_targets.push(target);
            }
            self.visit_stmt(statement);
            if review_target.is_some() {
                self.review_constructor_targets.pop();
            }
            if builder_target.is_some() {
                self.builder_usage_targets.pop();
            }
        }
        self.scopes.pop();
        self.block_depth -= 1;
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        let return_forbidden_types = match &item.sig.output {
            ReturnType::Default => Vec::new(),
            ReturnType::Type(_, type_) => self.forbidden_types_in_type(type_),
        };
        let top_level_item = self.function_frames.is_empty()
            && self.inline_module_depth == 0
            && self.block_depth == 0;
        self.function_frames.push(FunctionFrame {
            name: ident_name(&item.sig.ident),
            body_address: item.block.as_ref() as *const Block as usize,
            top_level_item,
            return_forbidden_types,
            return_scope_depth: self.return_scope_depth,
        });
        visit::visit_item_fn(self, item);
        self.function_frames.pop();
    }

    fn visit_local(&mut self, local: &'ast Local) {
        if let (Some(type_), Some(init)) = (explicit_pattern_type(&local.pat), &local.init) {
            let forbidden = self.forbidden_types_in_type(type_);
            self.record_typed_construction(&forbidden, &init.expr);
        }
        visit::visit_local(self, local);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        let impl_type = match item.self_ty.as_ref() {
            Type::Path(type_path) if type_path.qself.is_none() => path_segments(&type_path.path),
            _ => Vec::new(),
        };
        self.impl_types.push(impl_type);
        visit::visit_item_impl(self, item);
        self.impl_types.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        let return_forbidden_types = match &item.sig.output {
            ReturnType::Default => Vec::new(),
            ReturnType::Type(_, type_) => self.forbidden_types_in_type(type_),
        };
        self.function_frames.push(FunctionFrame {
            name: ident_name(&item.sig.ident),
            body_address: &item.block as *const Block as usize,
            top_level_item: false,
            return_forbidden_types,
            return_scope_depth: self.return_scope_depth,
        });
        visit::visit_impl_item_fn(self, item);
        self.function_frames.pop();
    }

    fn visit_expr_async(&mut self, expression: &'ast ExprAsync) {
        self.return_scope_depth += 1;
        visit::visit_expr_async(self, expression);
        self.return_scope_depth -= 1;
    }

    fn visit_expr_closure(&mut self, expression: &'ast ExprClosure) {
        self.return_scope_depth += 1;
        visit::visit_expr_closure(self, expression);
        self.return_scope_depth -= 1;
    }

    fn visit_expr_return(&mut self, expression: &'ast ExprReturn) {
        let type_names = self.function_frames.last().and_then(|frame| {
            (frame.return_scope_depth == self.return_scope_depth)
                .then(|| frame.return_forbidden_types.clone())
        });
        if let (Some(type_names), Some(value)) = (type_names, expression.expr.as_deref()) {
            self.record_typed_construction(&type_names, value);
        }
        visit::visit_expr_return(self, expression);
    }

    fn visit_expr_call(&mut self, expression: &'ast ExprCall) {
        if let Expr::Path(function) = expression.func.as_ref() {
            self.record_generic_factory_call(expression, function);
            if function.qself.is_none() {
                let original = path_segments(&function.path);
                let resolved = self.resolver().resolve(original.clone());
                if is_canonical_builder_path(&original)
                    && resolved.len() == 1
                    && resolved.first() == Some(&original)
                {
                    let usage = self.builder_usage(expression);
                    self.analysis.direct_builder_calls.push(BuilderCall {
                        path: original,
                        function: self.enclosing_function(),
                        direct_function_body: !matches!(usage, BuilderCallUse::Other)
                            && self.in_top_level_item_function(),
                        usage,
                    });
                }
            }
            self.record_expr_path(function, Some(expression));
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
        self.record_expr_path(expression, None);
        visit::visit_expr_path(self, expression);
    }

    fn visit_expr_struct(&mut self, expression: &'ast ExprStruct) {
        let original = path_segments(&expression.path);
        let mut expression_constructions = Vec::new();
        for resolved in self.resolver().resolve(original.clone()) {
            let Some((type_name, _)) = forbidden_type_in_path(&resolved) else {
                continue;
            };
            let construction = DirectConstruction {
                type_name,
                syntax: format!("{} {{ .. }}", original.join("::")),
                module_path: self.module_path.join("::"),
                enclosing_function: self.enclosing_function(),
                intentional_pr_review_constructor: false,
            };
            if !expression_constructions.contains(&construction) {
                expression_constructions.push(construction);
            }
        }
        self.analysis
            .direct_constructions
            .extend(expression_constructions);
        visit::visit_expr_struct(self, expression);
    }

    fn visit_macro(&mut self, macro_: &'ast Macro) {
        self.scan_macro_tokens(macro_);
        visit::visit_macro(self, macro_);
    }

    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        if use_tree_has_glob(&item.tree)
            && !(self.in_cfg_test_module && is_private_super_glob(item))
        {
            self.analysis.production_glob_imports += 1;
        }
        visit::visit_item_use(self, item);
    }
}

fn root_call(expression: &Expr) -> Option<&ExprCall> {
    match expression {
        Expr::Call(call) => Some(call),
        Expr::Group(group) => root_call(&group.expr),
        Expr::Paren(paren) => root_call(&paren.expr),
        Expr::Try(try_) => root_call(&try_.expr),
        _ => None,
    }
}

fn direct_builder_target(
    statement: &Stmt,
    is_tail_statement: bool,
) -> Option<(usize, BuilderCallUse)> {
    match statement {
        Stmt::Local(local) => {
            let binding = pattern_binding_name(&local.pat)?;
            let call = root_call(&local.init.as_ref()?.expr)?;
            Some((
                call as *const ExprCall as usize,
                BuilderCallUse::LetBinding(binding),
            ))
        }
        Stmt::Expr(expression, None) if is_tail_statement => root_call(expression).map(|call| {
            (
                call as *const ExprCall as usize,
                BuilderCallUse::TailExpression,
            )
        }),
        _ => None,
    }
}

fn pattern_binding_name(pattern: &Pat) -> Option<String> {
    match pattern {
        Pat::Ident(binding) => Some(ident_name(&binding.ident)),
        Pat::Type(type_) => pattern_binding_name(&type_.pat),
        _ => None,
    }
}

fn explicit_pattern_type(pattern: &Pat) -> Option<&Type> {
    let Pat::Type(type_) = pattern else {
        return None;
    };
    Some(&type_.ty)
}

fn direct_review_constructor_target(statement: &Stmt) -> Option<usize> {
    let expression = match statement {
        Stmt::Local(local) => &local.init.as_ref()?.expr,
        Stmt::Expr(expression, _) => expression,
        Stmt::Item(_) | Stmt::Macro(_) => return None,
    };
    transparent_receiver_call(expression).map(|call| call as *const ExprCall as usize)
}

fn transparent_receiver_call(expression: &Expr) -> Option<&ExprCall> {
    match expression {
        Expr::Call(call) => Some(call),
        Expr::MethodCall(call) => transparent_receiver_call(&call.receiver),
        Expr::Await(await_) => transparent_receiver_call(&await_.base),
        Expr::Group(group) => transparent_receiver_call(&group.expr),
        Expr::Paren(paren) => transparent_receiver_call(&paren.expr),
        Expr::Try(try_) => transparent_receiver_call(&try_.expr),
        _ => None,
    }
}

fn trait_constructor_syntax(expression: &Expr) -> Option<String> {
    let call = root_call(expression)?;
    let Expr::Path(function) = call.func.as_ref() else {
        return None;
    };
    if function.qself.is_some() {
        return None;
    }
    let segments = path_segments(&function.path);
    let is_trait_constructor = matches!(
        segments.as_slice(),
        [.., trait_name, method]
            if matches!(
                (trait_name.as_str(), method.as_str()),
                ("Default", "default") | ("From", "from") | ("TryFrom", "try_from")
            )
    );
    let is_zeroed = matches!(segments.as_slice(), [.., module, method]
        if module == "mem" && method == "zeroed");
    (is_trait_constructor || is_zeroed).then(|| segments.join("::"))
}

fn use_tree_has_glob(tree: &UseTree) -> bool {
    match tree {
        UseTree::Glob(_) => true,
        UseTree::Group(group) => group.items.iter().any(use_tree_has_glob),
        UseTree::Path(path) => use_tree_has_glob(&path.tree),
        UseTree::Name(_) | UseTree::Rename(_) => false,
    }
}

fn is_private_super_glob(item: &ItemUse) -> bool {
    matches!(item.vis, Visibility::Inherited)
        && matches!(
            &item.tree,
            UseTree::Path(path)
                if ident_name(&path.ident) == "super"
                    && matches!(path.tree.as_ref(), UseTree::Glob(_))
        )
}

fn is_canonical_builder_path(path: &[String]) -> bool {
    matches!(
        path,
        [agents, builder, function]
            if agents == "harness_agents"
                && builder == "builder"
                && matches!(
                    function.as_str(),
                    "registry_from_config" | "claude_agent_from_config"
                )
    )
}

fn forbidden_type_in_path(path: &[String]) -> Option<(String, Option<String>)> {
    path.iter()
        .enumerate()
        .rev()
        .find(|(_, segment)| FORBIDDEN_TYPES.contains(&segment.as_str()))
        .map(|(index, type_name)| {
            (
                type_name.clone(),
                path.get(index + 1..)
                    .and_then(|suffix| suffix.last())
                    .cloned(),
            )
        })
}

fn token_paths(stream: TokenStream, paths: &mut Vec<Segments>) {
    let tokens = stream.into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < tokens.len() {
        if let TokenTree::Group(group) = &tokens[index] {
            token_paths(group.stream(), paths);
            index += 1;
            continue;
        }
        let TokenTree::Ident(first) = &tokens[index] else {
            index += 1;
            continue;
        };
        let mut path = vec![ident_name(first)];
        let mut cursor = index + 1;
        while cursor + 2 < tokens.len()
            && matches!(&tokens[cursor], TokenTree::Punct(punct) if punct.as_char() == ':')
            && matches!(&tokens[cursor + 1], TokenTree::Punct(punct) if punct.as_char() == ':')
        {
            let TokenTree::Ident(segment) = &tokens[cursor + 2] else {
                break;
            };
            path.push(ident_name(segment));
            cursor += 3;
        }
        paths.push(path);
        index = cursor;
    }
}

fn intentional_codex_review_call(call: &ExprCall) -> bool {
    if call.args.len() != 2 {
        return false;
    }
    let mut arguments = call.args.iter();
    let Some(Expr::MethodCall(cli_path)) = arguments.next() else {
        return false;
    };
    let Expr::Field(field) = cli_path.receiver.as_ref() else {
        return false;
    };
    let Expr::Path(config) = field.base.as_ref() else {
        return false;
    };
    let first_matches = cli_path.method == "clone"
        && cli_path.args.is_empty()
        && matches!(&field.member, Member::Named(name) if name == "cli_path")
        && path_segments(&config.path) == ["review_config"];
    let Some(Expr::Path(sandbox)) = arguments.next() else {
        return false;
    };
    first_matches
        && path_segments(&sandbox.path)
            .ends_with(&["SandboxMode".to_string(), "ReadOnlyWithNetwork".to_string()])
}

pub(super) fn analyze_units(units: &[SourceUnit]) -> HashMap<PathBuf, SourceAnalysis> {
    let mut aliases_by_crate = HashMap::<String, CrateAliases>::new();
    let mut factories_by_crate = HashMap::<String, GenericFactories>::new();
    let test_modules = CfgTestModules::collect(units);
    for unit in units {
        collect_crate_aliases(
            &unit.file.items,
            &unit.module_path,
            aliases_by_crate.entry(unit.crate_id.clone()).or_default(),
        );
        collect_generic_factories(
            &unit.file.items,
            &unit.module_path,
            factories_by_crate.entry(unit.crate_id.clone()).or_default(),
        );
    }
    units
        .iter()
        .map(|unit| {
            let crate_aliases = aliases_by_crate
                .get(&unit.crate_id)
                .expect("source crate aliases exist");
            let generic_factories = factories_by_crate
                .get(&unit.crate_id)
                .expect("source generic factories exist");
            let mut scanner = SourceScanner::new(
                crate_aliases,
                generic_factories,
                unit.module_path.clone(),
                test_modules.contains(unit),
            );
            scanner.visit_file(&unit.file);
            (unit.relative.clone(), scanner.analysis)
        })
        .collect()
}
