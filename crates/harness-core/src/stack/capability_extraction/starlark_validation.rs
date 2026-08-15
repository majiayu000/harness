use anyhow::{anyhow, Result};
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::list::{ListRef, UnpackList};
use starlark::values::none::NoneType;
use starlark::values::Value;
use starlark_syntax::syntax::ast::{AstExpr, BinOp, Expr};
use starlark_syntax::syntax::module::AstModuleFields;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

const MAX_POLICY_SOURCE_BYTES: usize = 512 * 1024;
const MAX_STARLARK_CALLSTACK_SIZE: usize = 512;
const MAX_AST_NODES: usize = 1024;

#[derive(Debug)]
pub(super) enum ValidationError {
    Parse,
    Invalid,
}

#[derive(Debug, Clone)]
pub(super) struct ValidatedRule {
    pub(super) pattern: Pattern,
    pub(super) decision: String,
}

#[derive(Debug, ProvidesStaticType)]
struct Collector(RefCell<Vec<ValidatedRule>>);

pub(super) fn validate(
    identifier: &str,
    source: &str,
) -> std::result::Result<Vec<ValidatedRule>, ValidationError> {
    if source.len() > MAX_POLICY_SOURCE_BYTES {
        return Err(ValidationError::Invalid);
    }
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_load_reexport: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(identifier, source.to_owned(), &dialect)
        .map_err(|_| ValidationError::Parse)?;
    if !bounded_ast(&ast) {
        return Err(ValidationError::Invalid);
    }
    let globals = GlobalsBuilder::standard().with(policy_builtins).build();
    let module = starlark::environment::Module::new();
    let collector = Collector(RefCell::new(Vec::new()));
    let mut evaluator = Evaluator::new(&module);
    evaluator.extra = Some(&collector);
    evaluator
        .set_max_callstack_size(MAX_STARLARK_CALLSTACK_SIZE)
        .map_err(|_| ValidationError::Invalid)?;
    evaluator
        .eval_module(ast, &globals)
        .map_err(|_| ValidationError::Invalid)?;
    drop(evaluator);
    Ok(collector.0.into_inner())
}

fn bounded_ast(ast: &AstModule) -> bool {
    fn inspect(expr: &AstExpr, nodes: &mut usize) -> bool {
        *nodes += 1;
        if *nodes > MAX_AST_NODES {
            return false;
        }
        match &expr.node {
            Expr::Call(function, _) if !matches!(&function.node, Expr::Identifier(name) if matches!(name.ident.as_str(), "prefix_rule" | "host_executable")) => {
                return false
            }
            Expr::ListComprehension(..) | Expr::DictComprehension(..) | Expr::FString(..) => {
                return false
            }
            Expr::Op(_, operation, _)
                if !matches!(
                    operation,
                    BinOp::Or
                        | BinOp::And
                        | BinOp::Equal
                        | BinOp::NotEqual
                        | BinOp::Less
                        | BinOp::Greater
                        | BinOp::LessOrEqual
                        | BinOp::GreaterOrEqual
                        | BinOp::In
                        | BinOp::NotIn
                        | BinOp::Subtract
                ) =>
            {
                return false
            }
            _ => {}
        }
        let mut valid = true;
        expr.node.visit_expr(|child| valid &= inspect(child, nodes));
        valid
    }
    let mut nodes = 0;
    let mut valid = true;
    ast.statement()
        .visit_expr(|expr| valid &= inspect(expr, &mut nodes));
    valid
}

pub(super) type Pattern = Vec<Vec<String>>;

fn parse_pattern<'v>(pattern: UnpackList<Value<'v>>) -> Result<Pattern> {
    let tokens = pattern
        .items
        .into_iter()
        .map(|value| {
            if let Some(raw) = value.unpack_str() {
                return normalized(raw).map(|token| vec![token]);
            }
            let list = ListRef::from_value(value)
                .ok_or_else(|| anyhow!("pattern element must be a string or list"))?;
            let alternatives = list
                .content()
                .iter()
                .map(|item| {
                    item.unpack_str()
                        .ok_or_else(|| anyhow!("pattern alternatives must be strings"))
                        .and_then(normalized)
                })
                .collect::<Result<Vec<_>>>()?;
            if alternatives.is_empty() {
                return Err(anyhow!("pattern alternatives cannot be empty"));
            }
            Ok(alternatives)
        })
        .collect::<Result<Vec<_>>>()?;
    if tokens.is_empty() {
        return Err(anyhow!("pattern cannot be empty"));
    }
    Ok(tokens)
}

fn normalized(raw: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        Err(anyhow!("policy tokens cannot be empty"))
    } else {
        Ok(token.to_owned())
    }
}

fn parse_examples<'v>(examples: UnpackList<Value<'v>>) -> Result<Vec<Vec<String>>> {
    examples.items.into_iter().map(parse_example).collect()
}

fn parse_example<'v>(value: Value<'v>) -> Result<Vec<String>> {
    let tokens = if let Some(raw) = value.unpack_str() {
        shlex::split(raw).ok_or_else(|| anyhow!("example has invalid shell syntax"))?
    } else {
        let list = ListRef::from_value(value)
            .ok_or_else(|| anyhow!("example must be a string or list"))?;
        list.content()
            .iter()
            .map(|item| {
                item.unpack_str()
                    .ok_or_else(|| anyhow!("example entries must be strings"))
                    .and_then(normalized)
            })
            .collect::<Result<Vec<_>>>()?
    };
    if tokens.is_empty() {
        Err(anyhow!("example cannot be empty"))
    } else {
        Ok(tokens)
    }
}

fn pattern_matches(pattern: &Pattern, command: &[String]) -> bool {
    fn matches(pattern: &Pattern, command: &[String]) -> bool {
        command.len() >= pattern.len()
            && pattern
                .iter()
                .zip(command)
                .all(|(alternatives, token)| alternatives.iter().any(|item| item == token))
    }
    if matches(pattern, command) {
        return true;
    }
    let Some(program) = command
        .first()
        .filter(|program| Path::new(program).is_absolute())
    else {
        return false;
    };
    let Some(name) = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let resolved = std::iter::once(name.to_owned())
        .chain(command.iter().skip(1).cloned())
        .collect::<Vec<_>>();
    matches(pattern, &resolved)
}

fn validate_host_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    if name.is_empty()
        || path.components().count() != 1
        || path.file_name().and_then(|part| part.to_str()) != Some(name)
    {
        return Err(anyhow!("host executable name must be bare"));
    }
    Ok(())
}

#[starlark_module]
fn policy_builtins(builder: &mut GlobalsBuilder) {
    fn prefix_rule<'v>(
        pattern: UnpackList<Value<'v>>,
        decision: Option<&'v str>,
        r#match: Option<UnpackList<Value<'v>>>,
        not_match: Option<UnpackList<Value<'v>>>,
        justification: Option<&'v str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        if decision.is_some_and(|value| !matches!(value, "allow" | "prompt" | "forbidden")) {
            return Err(anyhow!("invalid decision"));
        }
        if justification.is_some_and(|value| value.trim().is_empty()) {
            return Err(anyhow!("justification cannot be empty"));
        }
        let pattern = parse_pattern(pattern)?;
        let matches = r#match.map(parse_examples).transpose()?.unwrap_or_default();
        let not_matches = not_match
            .map(parse_examples)
            .transpose()?
            .unwrap_or_default();
        if matches
            .iter()
            .any(|example| !pattern_matches(&pattern, example))
            || not_matches
                .iter()
                .any(|example| pattern_matches(&pattern, example))
        {
            return Err(anyhow!("policy examples do not match their declaration"));
        }
        let collector = eval
            .extra
            .as_ref()
            .and_then(|extra| extra.downcast_ref::<Collector>())
            .ok_or_else(|| anyhow!("policy collector missing"))?;
        collector.0.borrow_mut().push(ValidatedRule {
            pattern,
            decision: decision.unwrap_or("allow").to_owned(),
        });
        Ok(NoneType)
    }

    fn host_executable<'v>(
        name: &'v str,
        paths: UnpackList<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        validate_host_name(name)?;
        for raw in paths.items {
            let raw = raw
                .unpack_str()
                .ok_or_else(|| anyhow!("host executable paths must be strings"))?;
            let path = PathBuf::from(raw);
            if !path.is_absolute() || path.file_name().and_then(|part| part.to_str()) != Some(name)
            {
                return Err(anyhow!(
                    "host executable path must be absolute with matching basename"
                ));
            }
        }
        Ok(NoneType)
    }
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn validates_the_runtime_builtin_contract() {
        assert!(validate("valid.rules", r#"
prefix_rule(pattern = ["git", "push"], decision = "prompt", match = ["/usr/bin/git push"], not_match = ["bin/git push"])
prefix_rule(pattern = ["/usr/bin/git", "push"], match = ["/usr/bin/git push"])
host_executable(name = "git", paths = ["/usr/bin/git"])
"#).is_ok());
        for invalid in [
            "prefix_rule(pattern = [\"curl\"]); unknown_builtin()",
            "prefix_rule(pattern = [unknown_name])",
            "prefix_rule(pattern = [\"curl\"], not_match = [\"curl\"])",
            "host_executable(name = \"git\", paths = [\"relative/git\"])",
            "host_executable(name = \"git\", paths = [\"/usr/bin/curl\"])",
            "prefix_rule(pattern = [\"curl\"] * 1000000000)",
            "prefix_rule(pattern = [item for item in [\"curl\"]])",
            "x = \"a\"; x = x + x; prefix_rule(pattern = [x])",
        ] {
            assert!(
                validate("invalid.rules", invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}
