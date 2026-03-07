use crate::exec_policy::executable_name;
use crate::exec_policy::{
    ExecDecision, ExecPolicy, MatchOptions, PatternToken, PrefixPattern, PrefixRule,
};
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::AstModule;
use starlark::syntax::Dialect;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::Value;
use std::cell::{RefCell, RefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_POLICY_SOURCE_BYTES: usize = 512 * 1024;
const MAX_STARLARK_CALLSTACK_SIZE: usize = 512;

pub struct ExecPolicyParser {
    builder: RefCell<PolicyBuilder>,
}

impl Default for ExecPolicyParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecPolicyParser {
    pub fn new() -> Self {
        Self {
            builder: RefCell::new(PolicyBuilder::new()),
        }
    }

    pub fn parse(&mut self, identifier: &str, policy_contents: &str) -> anyhow::Result<()> {
        if policy_contents.len() > MAX_POLICY_SOURCE_BYTES {
            return Err(anyhow!(
                "starlark policy {} exceeds {} bytes",
                identifier,
                MAX_POLICY_SOURCE_BYTES
            ));
        }

        let pending_count = self.builder.borrow().pending_example_validations.len();
        let dialect = Dialect {
            enable_def: false,
            enable_lambda: false,
            enable_load: false,
            enable_load_reexport: false,
            enable_top_level_stmt: false,
            ..Dialect::Standard
        };
        let ast = AstModule::parse(identifier, policy_contents.to_string(), &dialect)
            .map_err(|error| anyhow!("starlark parse failed for {identifier}: {error}"))?;
        let globals = GlobalsBuilder::standard().with(policy_builtins).build();
        let module = Module::new();
        {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&self.builder);
            eval.set_max_callstack_size(MAX_STARLARK_CALLSTACK_SIZE)
                .map_err(|error| anyhow!("failed to configure evaluator limits: {error}"))?;
            eval.eval_module(ast, &globals)
                .map_err(|error| anyhow!("starlark evaluation failed for {identifier}: {error}"))?;
        }
        self.builder
            .borrow()
            .validate_pending_examples_from(pending_count)?;
        Ok(())
    }

    pub fn build(self) -> ExecPolicy {
        self.builder.into_inner().build()
    }
}

#[derive(Debug, ProvidesStaticType)]
struct PolicyBuilder {
    policy: ExecPolicy,
    pending_example_validations: Vec<PendingExampleValidation>,
}

impl PolicyBuilder {
    fn new() -> Self {
        Self {
            policy: ExecPolicy::empty(),
            pending_example_validations: Vec::new(),
        }
    }

    fn add_rule(&mut self, rule: PrefixRule) {
        self.policy.add_rule(rule);
    }

    fn add_host_executable(&mut self, name: String, paths: Vec<PathBuf>) {
        self.policy.set_host_executable_paths(name, paths);
    }

    fn add_pending_example_validation(
        &mut self,
        rules: Vec<PrefixRule>,
        matches: Vec<Vec<String>>,
        not_matches: Vec<Vec<String>>,
    ) {
        self.pending_example_validations
            .push(PendingExampleValidation {
                rules,
                matches,
                not_matches,
            });
    }

    fn validate_pending_examples_from(&self, start: usize) -> anyhow::Result<()> {
        for validation in &self.pending_example_validations[start..] {
            let mut policy = ExecPolicy::empty();
            for rule in &validation.rules {
                policy.add_rule(rule.clone());
            }
            let options = MatchOptions {
                resolve_host_executables: true,
            };

            for example in &validation.matches {
                if policy
                    .matches_for_command_with_options(example, &options)
                    .is_empty()
                {
                    let rendered = shlex::try_join(example.iter().map(String::as_str))
                        .unwrap_or_else(|_| "invalid example".to_string());
                    return Err(anyhow!("example did not match rule: {rendered}"));
                }
            }

            for example in &validation.not_matches {
                if !policy
                    .matches_for_command_with_options(example, &options)
                    .is_empty()
                {
                    let rendered = shlex::try_join(example.iter().map(String::as_str))
                        .unwrap_or_else(|_| "invalid example".to_string());
                    return Err(anyhow!(
                        "negative example unexpectedly matched rule: {rendered}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn build(self) -> ExecPolicy {
        self.policy
    }
}

#[derive(Debug)]
struct PendingExampleValidation {
    rules: Vec<PrefixRule>,
    matches: Vec<Vec<String>>,
    not_matches: Vec<Vec<String>>,
}

fn policy_builder<'v, 'a>(
    eval: &Evaluator<'v, 'a, '_>,
) -> anyhow::Result<RefMut<'a, PolicyBuilder>> {
    let builder_cell = eval
        .extra
        .as_ref()
        .and_then(|extra| extra.downcast_ref::<RefCell<PolicyBuilder>>())
        .ok_or_else(|| anyhow!("internal error: policy builder missing in evaluator context"))?;
    Ok(builder_cell.borrow_mut())
}

fn parse_decision(raw: &str) -> anyhow::Result<ExecDecision> {
    match raw {
        "allow" => Ok(ExecDecision::Allow),
        "prompt" => Ok(ExecDecision::Prompt),
        "forbidden" => Ok(ExecDecision::Forbidden),
        other => Err(anyhow!(
            "invalid decision `{other}`; expected allow|prompt|forbidden"
        )),
    }
}

fn parse_pattern<'v>(pattern: UnpackList<Value<'v>>) -> anyhow::Result<Vec<PatternToken>> {
    let tokens: Vec<PatternToken> = pattern
        .items
        .into_iter()
        .map(parse_pattern_token)
        .collect::<anyhow::Result<_>>()?;
    if tokens.is_empty() {
        return Err(anyhow!("pattern cannot be empty"));
    }
    Ok(tokens)
}

fn parse_pattern_token<'v>(value: Value<'v>) -> anyhow::Result<PatternToken> {
    if let Some(raw) = value.unpack_str() {
        let normalized = raw.trim();
        if normalized.is_empty() {
            return Err(anyhow!("pattern token cannot be empty"));
        }
        return Ok(PatternToken::Single(normalized.to_string()));
    }

    let Some(list) = ListRef::from_value(value) else {
        return Err(anyhow!(
            "pattern element must be a string or list of strings (got {})",
            value.get_type()
        ));
    };

    let alternatives: Vec<String> = list
        .content()
        .iter()
        .map(|item| {
            let Some(token) = item.unpack_str() else {
                return Err(anyhow!(
                    "pattern alternatives must be strings (got {})",
                    item.get_type()
                ));
            };
            let normalized = token.trim();
            if normalized.is_empty() {
                return Err(anyhow!("pattern alternatives cannot contain empty tokens"));
            }
            Ok(normalized.to_string())
        })
        .collect::<anyhow::Result<_>>()?;

    match alternatives.as_slice() {
        [] => Err(anyhow!("pattern alternatives cannot be empty")),
        [single] => Ok(PatternToken::Single(single.clone())),
        _ => Ok(PatternToken::Alts(alternatives)),
    }
}

fn parse_examples<'v>(examples: UnpackList<Value<'v>>) -> anyhow::Result<Vec<Vec<String>>> {
    examples.items.into_iter().map(parse_example).collect()
}

fn parse_example<'v>(value: Value<'v>) -> anyhow::Result<Vec<String>> {
    if let Some(raw) = value.unpack_str() {
        let Some(tokens) = shlex::split(raw) else {
            return Err(anyhow!("example string has invalid shell syntax"));
        };
        if tokens.is_empty() {
            return Err(anyhow!("example string cannot be empty"));
        }
        return Ok(tokens);
    }

    let Some(list) = ListRef::from_value(value) else {
        return Err(anyhow!(
            "example must be a string or list of strings (got {})",
            value.get_type()
        ));
    };

    let tokens: Vec<String> = list
        .content()
        .iter()
        .map(|item| {
            let token = item
                .unpack_str()
                .ok_or_else(|| anyhow!("example list entries must be strings"))?;
            let normalized = token.trim();
            if normalized.is_empty() {
                return Err(anyhow!("example list entries cannot be empty"));
            }
            Ok(normalized.to_string())
        })
        .collect::<anyhow::Result<_>>()?;
    if tokens.is_empty() {
        return Err(anyhow!("example list cannot be empty"));
    }
    Ok(tokens)
}

fn validate_host_executable_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        return Err(anyhow!("host_executable name cannot be empty"));
    }
    let path = Path::new(name);
    if path.components().count() != 1
        || path.file_name().and_then(|token| token.to_str()) != Some(name)
    {
        return Err(anyhow!(
            "host_executable name must be a bare executable name: {name}"
        ));
    }
    Ok(())
}

fn parse_literal_absolute_path(raw: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(anyhow!("host_executable paths must be absolute: {raw}"));
    }
    Ok(path)
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
        let decision = decision
            .map(parse_decision)
            .transpose()?
            .unwrap_or(ExecDecision::Allow);
        let justification = justification
            .map(|raw| {
                if raw.trim().is_empty() {
                    Err(anyhow!("justification cannot be empty"))
                } else {
                    Ok(raw.to_string())
                }
            })
            .transpose()?;

        let pattern_tokens = parse_pattern(pattern)?;
        let matches = r#match.map(parse_examples).transpose()?.unwrap_or_default();
        let not_matches = not_match
            .map(parse_examples)
            .transpose()?
            .unwrap_or_default();

        let mut policy_builder = policy_builder(eval)?;
        let (first_token, remaining_tokens) = pattern_tokens
            .split_first()
            .ok_or_else(|| anyhow!("pattern cannot be empty"))?;

        let rest: Arc<[PatternToken]> = remaining_tokens.to_vec().into();
        let rules: Vec<PrefixRule> = first_token
            .alternatives()
            .iter()
            .map(|head| PrefixRule {
                pattern: PrefixPattern {
                    first: Arc::from(head.as_str()),
                    rest: rest.clone(),
                },
                decision,
                justification: justification.clone(),
            })
            .collect();

        for rule in rules.iter().cloned() {
            policy_builder.add_rule(rule);
        }
        policy_builder.add_pending_example_validation(rules, matches, not_matches);
        Ok(NoneType)
    }

    fn host_executable<'v>(
        name: &'v str,
        paths: UnpackList<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        validate_host_executable_name(name)?;
        let mut parsed_paths = Vec::new();
        for item in paths.items {
            let raw = item
                .unpack_str()
                .ok_or_else(|| anyhow!("host_executable paths must be strings"))?;
            let path = parse_literal_absolute_path(raw)?;
            if executable_name(&path) != Some(name) {
                return Err(anyhow!(
                    "host_executable path `{raw}` must have basename `{name}`"
                ));
            }
            if !parsed_paths.iter().any(|existing| existing == &path) {
                parsed_paths.push(path);
            }
        }
        policy_builder(eval)?.add_host_executable(name.to_string(), parsed_paths);
        Ok(NoneType)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_supports_prefix_rule_and_matches_command() -> anyhow::Result<()> {
        let mut parser = ExecPolicyParser::new();
        parser.parse(
            "inline",
            r#"
prefix_rule(
  pattern = ["git", ["push", "pull"]],
  decision = "prompt",
  justification = "needs approval",
  match = ["git push"],
  not_match = ["git status"],
)
"#,
        )?;
        let policy = parser.build();
        let result = policy.check_command(
            &["git".to_string(), "push".to_string()],
            &MatchOptions::default(),
        );
        assert_eq!(result.decision, Some(ExecDecision::Prompt));
        assert_eq!(result.matched_rules.len(), 1);
        Ok(())
    }

    #[test]
    fn parser_validates_negative_examples() {
        let mut parser = ExecPolicyParser::new();
        let result = parser.parse(
            "inline",
            r#"
prefix_rule(
  pattern = ["git", "push"],
  decision = "prompt",
  not_match = ["git push"],
)
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parser_trims_pattern_tokens() -> anyhow::Result<()> {
        let mut parser = ExecPolicyParser::new();
        parser.parse(
            "inline",
            r#"
prefix_rule(
  pattern = [" git ", [" push ", " pull "]],
  decision = "prompt",
)
"#,
        )?;
        let policy = parser.build();
        let result = policy.check_command(
            &["git".to_string(), "push".to_string()],
            &MatchOptions::default(),
        );
        assert_eq!(result.decision, Some(ExecDecision::Prompt));
        Ok(())
    }

    #[test]
    fn host_executable_resolution_respects_declared_paths() -> anyhow::Result<()> {
        let mut parser = ExecPolicyParser::new();
        parser.parse(
            "inline",
            r#"
prefix_rule(pattern = ["git", "status"], decision = "allow")
host_executable(name = "git", paths = ["/usr/bin/git"])
"#,
        )?;
        let policy = parser.build();

        let matched = policy.check_command(
            &[
                "/usr/bin/git".to_string(),
                "status".to_string(),
                "--short".to_string(),
            ],
            &MatchOptions {
                resolve_host_executables: true,
            },
        );
        assert_eq!(matched.decision, Some(ExecDecision::Allow));
        assert_eq!(matched.matched_rules.len(), 1);

        let unmatched = policy.check_command(
            &[
                "/opt/homebrew/bin/git".to_string(),
                "status".to_string(),
                "--short".to_string(),
            ],
            &MatchOptions {
                resolve_host_executables: true,
            },
        );
        assert!(unmatched.matched_rules.is_empty());
        assert_eq!(unmatched.decision, None);
        Ok(())
    }

    #[test]
    fn parser_rejects_load_statements_when_disabled() {
        let mut parser = ExecPolicyParser::new();
        let error = parser
            .parse("inline", "load(\"foo.star\", \"x\")")
            .expect_err("load must be rejected");
        assert!(error.to_string().contains("load"));
    }

    #[test]
    fn parser_rejects_function_definitions() {
        let mut parser = ExecPolicyParser::new();
        let error = parser
            .parse(
                "inline",
                r#"
def ignored():
  pass
"#,
            )
            .expect_err("function definitions should be rejected");
        assert!(error.to_string().contains("parse failed"));
    }

    #[test]
    fn parser_rejects_top_level_for_statements() {
        let mut parser = ExecPolicyParser::new();
        let error = parser
            .parse(
                "inline",
                r#"
for name in ["git"]:
  prefix_rule(pattern = [name, "status"], decision = "allow")
"#,
            )
            .expect_err("top-level for statements should be rejected");
        assert!(error.to_string().contains("for"));
    }

    #[test]
    fn parser_rejects_while_keyword() {
        let mut parser = ExecPolicyParser::new();
        let error = parser
            .parse(
                "inline",
                r#"
while True:
  pass
"#,
            )
            .expect_err("while should be rejected");
        let message = error.to_string();
        assert!(message.contains("while"));
        assert!(message.contains("reserved keyword"));
    }
}
