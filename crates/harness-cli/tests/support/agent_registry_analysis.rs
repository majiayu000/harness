//! Structural guard for configured agent construction in `harness-cli`, with one
//! exact exception for the read-only Codex PR reviewer.

use proc_macro2::{TokenStream, TokenTree};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
};
use syn::{
    visit::{self, Visit},
    Block, Expr, ExprCall, ExprPath, ExprStruct, File, Item, ItemFn, ItemImpl, ItemMod, ItemType,
    ItemUse, Macro, Member, Path as SynPath, Stmt, Type, UseTree,
};

type Segments = Vec<String>;

const REGISTRY_BUILDER: &str = "harness_agents::builder::registry_from_config";
const CLAUDE_BUILDER: &str = "harness_agents::builder::claude_agent_from_config";
pub(super) const REQUIRED_BUILDER_CALLS: [(&str, &str); 5] = [
    ("src/commands/serve.rs", REGISTRY_BUILDER),
    ("src/commands/exec.rs", REGISTRY_BUILDER),
    ("src/gc.rs", REGISTRY_BUILDER),
    ("src/cmd/mcp_server.rs", REGISTRY_BUILDER),
    ("src/cmd/pr.rs", CLAUDE_BUILDER),
];

const FORBIDDEN_TYPES: [&str; 7] = [
    "AgentRegistry",
    "ClaudeCodeAgent",
    "CodexAgent",
    "ClaudeAdapter",
    "CodexAdapter",
    "AnthropicApiAgent",
    "ProviderBackpressureGate",
];

pub(super) const ALLOWED_DIRECT_CONSTRUCTION_PATH: &str = "src/cmd/pr.rs";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AliasTarget {
    path: Segments,
    module_path: Segments,
}

#[derive(Clone, Debug, Default)]
struct AliasScope {
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
struct CrateAliases {
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

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DirectConstruction {
    pub(super) type_name: String,
    pub(super) syntax: String,
    pub(super) module_path: String,
    pub(super) enclosing_function: Option<String>,
    pub(super) intentional_pr_review_constructor: bool,
    associated_item: Option<String>,
    is_call: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MacroViolation {
    pub(super) macro_path: String,
    pub(super) forbidden_type: String,
}

#[derive(Default)]
pub(super) struct SourceAnalysis {
    direct_builder_calls: Vec<Segments>,
    pub(super) direct_constructions: Vec<DirectConstruction>,
    pub(super) macro_violations: Vec<MacroViolation>,
}

impl SourceAnalysis {
    pub(super) fn direct_builder_call_count(&self, expected_path: &str) -> usize {
        let expected = expected_path.split("::").collect::<Vec<_>>();
        self.direct_builder_calls
            .iter()
            .filter(|path| path.iter().map(String::as_str).eq(expected.iter().copied()))
            .count()
    }
}

struct SourceUnit {
    relative: PathBuf,
    module_path: Segments,
    file: File,
}

struct Resolver<'a> {
    crate_aliases: &'a CrateAliases,
    scopes: &'a [AliasScope],
    module_path: &'a [String],
    impl_types: &'a [Segments],
}

impl Resolver<'_> {
    fn resolve(&self, mut path: Segments) -> Vec<Segments> {
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

struct SourceScanner<'a> {
    crate_aliases: &'a CrateAliases,
    scopes: Vec<AliasScope>,
    module_path: Segments,
    impl_types: Vec<Segments>,
    function_names: Vec<String>,
    analysis: SourceAnalysis,
}

impl<'a> SourceScanner<'a> {
    fn new(crate_aliases: &'a CrateAliases, module_path: Segments) -> Self {
        Self {
            crate_aliases,
            scopes: Vec::new(),
            module_path,
            impl_types: Vec::new(),
            function_names: Vec::new(),
            analysis: SourceAnalysis::default(),
        }
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            crate_aliases: self.crate_aliases,
            scopes: &self.scopes,
            module_path: &self.module_path,
            impl_types: &self.impl_types,
        }
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
        for candidate in candidates {
            let Some((type_name, associated_item)) = forbidden_type_in_path(&candidate) else {
                continue;
            };
            let construction = DirectConstruction {
                intentional_pr_review_constructor: type_name == "CodexAgent"
                    && associated_item.as_deref() == Some("new")
                    && original == "CodexAgent::new"
                    && call.is_some_and(intentional_codex_review_call)
                    && self
                        .function_names
                        .last()
                        .is_some_and(|name| name == "review"),
                type_name,
                associated_item,
                syntax: original.clone(),
                module_path: self.module_path.join("::"),
                is_call: call.is_some(),
                enclosing_function: self.function_names.last().cloned(),
            };
            if !self.analysis.direct_constructions.contains(&construction) {
                self.analysis.direct_constructions.push(construction);
            }
        }
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
        self.module_path.push(ident_name(&item.ident));
        self.scopes
            .push(alias_scope_from_items(items, &self.module_path));
        for nested in items {
            self.visit_item(nested);
        }
        self.scopes.clear();
        self.module_path.pop();
        self.scopes = parent_scopes;
    }

    fn visit_block(&mut self, block: &'ast Block) {
        self.scopes
            .push(alias_scope_from_block(block, &self.module_path));
        for statement in &block.stmts {
            self.visit_stmt(statement);
        }
        self.scopes.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.function_names.push(ident_name(&item.sig.ident));
        visit::visit_item_fn(self, item);
        self.function_names.pop();
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

    fn visit_expr_call(&mut self, expression: &'ast ExprCall) {
        if let Expr::Path(function) = expression.func.as_ref() {
            if function.qself.is_none() {
                let original = path_segments(&function.path);
                if is_canonical_builder_path(&original)
                    && self
                        .resolver()
                        .resolve(original.clone())
                        .iter()
                        .any(|resolved| resolved == &original)
                {
                    self.analysis.direct_builder_calls.push(original);
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
        for resolved in self.resolver().resolve(original.clone()) {
            let Some((type_name, _)) = forbidden_type_in_path(&resolved) else {
                continue;
            };
            let construction = DirectConstruction {
                type_name,
                associated_item: None,
                syntax: format!("{} {{ .. }}", original.join("::")),
                module_path: self.module_path.join("::"),
                is_call: false,
                enclosing_function: self.function_names.last().cloned(),
                intentional_pr_review_constructor: false,
            };
            if !self.analysis.direct_constructions.contains(&construction) {
                self.analysis.direct_constructions.push(construction);
            }
        }
        visit::visit_expr_struct(self, expression);
    }

    fn visit_macro(&mut self, macro_: &'ast Macro) {
        self.scan_macro_tokens(macro_);
        visit::visit_macro(self, macro_);
    }
}

fn path_segments(path: &SynPath) -> Segments {
    path.segments
        .iter()
        .map(|segment| ident_name(&segment.ident))
        .collect()
}

fn ident_name(ident: &proc_macro2::Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_string()
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

fn alias_scope_from_items(items: &[Item], module_path: &[String]) -> AliasScope {
    let mut scope = AliasScope::default();
    for item in items {
        add_item_alias(item, &mut scope, module_path);
    }
    scope
}

fn alias_scope_from_block(block: &Block, module_path: &[String]) -> AliasScope {
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

fn collect_crate_aliases(items: &[Item], module_path: &[String], aliases: &mut CrateAliases) {
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

fn source_module_path(relative: &Path) -> Segments {
    let relative = relative.strip_prefix("src").unwrap_or(relative);
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

fn analyze_units(units: &[SourceUnit]) -> HashMap<PathBuf, SourceAnalysis> {
    let mut crate_aliases = CrateAliases::default();
    for unit in units {
        collect_crate_aliases(&unit.file.items, &unit.module_path, &mut crate_aliases);
    }
    units
        .iter()
        .map(|unit| {
            let mut scanner = SourceScanner::new(&crate_aliases, unit.module_path.clone());
            scanner.visit_file(&unit.file);
            (unit.relative.clone(), scanner.analysis)
        })
        .collect()
}

pub(super) fn analyze_source(source: &str) -> syn::Result<SourceAnalysis> {
    let unit = SourceUnit {
        relative: PathBuf::from("src/main.rs"),
        module_path: Vec::new(),
        file: syn::parse_file(source)?,
    };
    Ok(analyze_units(&[unit])
        .remove(Path::new("src/main.rs"))
        .expect("single source analysis exists"))
}

pub(super) fn analyze_source_set(
    sources: &[(&str, &str)],
) -> syn::Result<HashMap<PathBuf, SourceAnalysis>> {
    let mut units = Vec::new();
    for (relative, source) in sources {
        let relative = PathBuf::from(relative);
        units.push(SourceUnit {
            module_path: source_module_path(&relative),
            relative,
            file: syn::parse_file(source)?,
        });
    }
    Ok(analyze_units(&units))
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

pub(super) fn analyze_cli_sources() -> HashMap<PathBuf, SourceAnalysis> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_sources(&crate_dir.join("src"), &mut paths);
    paths.sort();
    let units = paths
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(&crate_dir)
                .expect("CLI source stays within crate")
                .to_path_buf();
            let source = std::fs::read_to_string(&path).expect("readable CLI source");
            SourceUnit {
                module_path: source_module_path(&relative),
                relative,
                file: syn::parse_file(&source)
                    .unwrap_or_else(|error| panic!("{} should parse: {error}", path.display())),
            }
        })
        .collect::<Vec<_>>();
    analyze_units(&units)
}
