use anyhow::anyhow;
use anyhow::Context;
use serde::{Deserialize, Serialize};
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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecDecision {
    Allow,
    Prompt,
    Forbidden,
}

impl ExecDecision {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "allow" => Ok(Self::Allow),
            "prompt" => Ok(Self::Prompt),
            "forbidden" => Ok(Self::Forbidden),
            other => Err(anyhow!(
                "invalid decision `{other}`; expected allow|prompt|forbidden"
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    Single(String),
    Alts(Vec<String>),
}

impl PatternToken {
    fn matches(&self, token: &str) -> bool {
        match self {
            Self::Single(expected) => expected == token,
            Self::Alts(alternatives) => alternatives.iter().any(|alt| alt == token),
        }
    }

    fn alternatives(&self) -> &[String] {
        match self {
            Self::Single(expected) => std::slice::from_ref(expected),
            Self::Alts(alternatives) => alternatives,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixPattern {
    pub first: Arc<str>,
    pub rest: Arc<[PatternToken]>,
}

impl PrefixPattern {
    fn matches_prefix(&self, command: &[String]) -> Option<Vec<String>> {
        let pattern_len = self.rest.len() + 1;
        if command.len() < pattern_len || command[0] != self.first.as_ref() {
            return None;
        }

        for (pattern_token, command_token) in self.rest.iter().zip(&command[1..pattern_len]) {
            if !pattern_token.matches(command_token) {
                return None;
            }
        }

        Some(command[..pattern_len].to_vec())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixRule {
    pub pattern: PrefixPattern,
    pub decision: ExecDecision,
    pub justification: Option<String>,
}

impl PrefixRule {
    fn matches(&self, command: &[String]) -> Option<RuleMatch> {
        self.pattern
            .matches_prefix(command)
            .map(|matched_prefix| RuleMatch::PrefixRuleMatch {
                matched_prefix,
                decision: self.decision,
                resolved_program: None,
                justification: self.justification.clone(),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleMatch {
    PrefixRuleMatch {
        #[serde(rename = "matchedPrefix")]
        matched_prefix: Vec<String>,
        decision: ExecDecision,
        #[serde(rename = "resolvedProgram", skip_serializing_if = "Option::is_none")]
        resolved_program: Option<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        justification: Option<String>,
    },
}

impl RuleMatch {
    fn decision(&self) -> ExecDecision {
        match self {
            Self::PrefixRuleMatch { decision, .. } => *decision,
        }
    }

    fn with_resolved_program(self, program: &Path) -> Self {
        match self {
            Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                justification,
                ..
            } => Self::PrefixRuleMatch {
                matched_prefix,
                decision,
                resolved_program: Some(program.to_path_buf()),
                justification,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct MatchOptions {
    pub resolve_host_executables: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ExecPolicy {
    rules_by_program: HashMap<String, Vec<PrefixRule>>,
    host_executables_by_name: HashMap<String, Arc<[PathBuf]>>,
}

impl ExecPolicy {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn merge_overlay(&self, overlay: &ExecPolicy) -> ExecPolicy {
        let mut combined = self.clone();
        for (program, rules) in &overlay.rules_by_program {
            combined
                .rules_by_program
                .entry(program.clone())
                .or_default()
                .extend(rules.iter().cloned());
        }
        combined
            .host_executables_by_name
            .extend(overlay.host_executables_by_name.clone());
        combined
    }

    pub fn add_rule(&mut self, rule: PrefixRule) {
        self.rules_by_program
            .entry(rule.pattern.first.as_ref().to_string())
            .or_default()
            .push(rule);
    }

    pub fn set_host_executable_paths(&mut self, name: String, paths: Vec<PathBuf>) {
        self.host_executables_by_name.insert(name, paths.into());
    }

    pub fn check_command(
        &self,
        command: &[String],
        options: &MatchOptions,
    ) -> ExecPolicyCheckOutput {
        let matched_rules = self.matches_for_command_with_options(command, options);
        ExecPolicyCheckOutput::from_matches(matched_rules)
    }

    pub fn matches_for_command_with_options(
        &self,
        command: &[String],
        options: &MatchOptions,
    ) -> Vec<RuleMatch> {
        let exact_matches = self.match_exact_rules(command);
        if !exact_matches.is_empty() {
            return exact_matches;
        }

        if options.resolve_host_executables {
            return self.match_host_executable_rules(command);
        }

        Vec::new()
    }

    fn match_exact_rules(&self, command: &[String]) -> Vec<RuleMatch> {
        let Some(program) = command.first() else {
            return Vec::new();
        };

        self.rules_by_program
            .get(program)
            .map(|rules| {
                rules
                    .iter()
                    .filter_map(|rule| rule.matches(command))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn match_host_executable_rules(&self, command: &[String]) -> Vec<RuleMatch> {
        let Some(program) = command.first() else {
            return Vec::new();
        };

        let program_path = Path::new(program);
        if !program_path.is_absolute() {
            return Vec::new();
        }

        let Some(name) = executable_name(program_path) else {
            return Vec::new();
        };

        let Some(rules) = self.rules_by_program.get(name) else {
            return Vec::new();
        };

        if let Some(paths) = self.host_executables_by_name.get(name) {
            let raw_program_path = PathBuf::from(program);
            if !paths.iter().any(|path| path == &raw_program_path) {
                return Vec::new();
            }
        }

        let basename_command = std::iter::once(name.to_string())
            .chain(command.iter().skip(1).cloned())
            .collect::<Vec<_>>();
        let resolved_path = PathBuf::from(program);
        rules
            .iter()
            .filter_map(|rule| rule.matches(&basename_command))
            .map(|rule_match| rule_match.with_resolved_program(&resolved_path))
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecPolicyCheckOutput {
    #[serde(rename = "matchedRules")]
    pub matched_rules: Vec<RuleMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<ExecDecision>,
}

impl ExecPolicyCheckOutput {
    fn from_matches(matched_rules: Vec<RuleMatch>) -> Self {
        let decision = matched_rules.iter().map(RuleMatch::decision).max();
        Self {
            matched_rules,
            decision,
        }
    }
}

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
        let pending_count = self.builder.borrow().pending_example_validations.len();
        let mut dialect = Dialect::Extended.clone();
        dialect.enable_f_strings = true;
        let ast = AstModule::parse(identifier, policy_contents.to_string(), &dialect)
            .map_err(|error| anyhow!("starlark parse failed for {identifier}: {error}"))?;
        let globals = GlobalsBuilder::standard().with(policy_builtins).build();
        let module = Module::new();
        {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&self.builder);
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
            policy.host_executables_by_name = self.policy.host_executables_by_name.clone();
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

fn policy_builder<'v, 'a>(eval: &Evaluator<'v, 'a, '_>) -> RefMut<'a, PolicyBuilder> {
    #[expect(clippy::expect_used)]
    eval.extra
        .as_ref()
        .expect("policy builder requires Evaluator.extra to be populated")
        .downcast_ref::<RefCell<PolicyBuilder>>()
        .expect("Evaluator.extra must contain a PolicyBuilder")
        .borrow_mut()
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
        if raw.trim().is_empty() {
            return Err(anyhow!("pattern token cannot be empty"));
        }
        return Ok(PatternToken::Single(raw.to_string()));
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
            if token.trim().is_empty() {
                return Err(anyhow!("pattern alternatives cannot contain empty tokens"));
            }
            Ok(token.to_string())
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
            item.unpack_str()
                .ok_or_else(|| anyhow!("example list entries must be strings"))
                .map(str::to_string)
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

fn executable_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
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
            .map(ExecDecision::parse)
            .transpose()?
            .unwrap_or(ExecDecision::Allow);
        let justification = match justification {
            Some(raw) if raw.trim().is_empty() => {
                return Err(anyhow!("justification cannot be empty"));
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        let pattern_tokens = parse_pattern(pattern)?;
        let matches = r#match.map(parse_examples).transpose()?.unwrap_or_default();
        let not_matches = not_match
            .map(parse_examples)
            .transpose()?
            .unwrap_or_default();

        let mut policy_builder = policy_builder(eval);
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
        policy_builder(eval).add_host_executable(name.to_string(), parsed_paths);
        Ok(NoneType)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequirementsToml {
    pub rules: RequirementsRuleSetToml,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequirementsRuleSetToml {
    #[serde(default)]
    pub prefix_rules: Vec<RequirementsPrefixRuleToml>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequirementsPrefixRuleToml {
    pub pattern: Vec<RequirementsPatternTokenToml>,
    pub decision: Option<RequirementsDecisionToml>,
    pub justification: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequirementsPatternTokenToml {
    pub token: Option<String>,
    pub any_of: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequirementsDecisionToml {
    Allow,
    Prompt,
    Forbidden,
}

impl RequirementsDecisionToml {
    fn as_decision(self) -> ExecDecision {
        match self {
            Self::Allow => ExecDecision::Allow,
            Self::Prompt => ExecDecision::Prompt,
            Self::Forbidden => ExecDecision::Forbidden,
        }
    }
}

impl RequirementsToml {
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read requirements file {}", path.display()))?;
        toml::from_str::<Self>(&content)
            .with_context(|| format!("failed to parse requirements file {}", path.display()))
    }

    pub fn to_policy(&self) -> anyhow::Result<ExecPolicy> {
        if self.rules.prefix_rules.is_empty() {
            return Err(anyhow!("rules.prefix_rules cannot be empty"));
        }

        let mut policy = ExecPolicy::empty();
        for (rule_index, rule) in self.rules.prefix_rules.iter().enumerate() {
            if let Some(justification) = &rule.justification {
                if justification.trim().is_empty() {
                    return Err(anyhow!(
                        "rules.prefix_rules[{rule_index}].justification cannot be empty"
                    ));
                }
            }
            if rule.pattern.is_empty() {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern cannot be empty"
                ));
            }

            let decision = match rule.decision {
                Some(RequirementsDecisionToml::Allow) => {
                    return Err(anyhow!(
                        "rules.prefix_rules[{rule_index}] decision `allow` is not permitted in requirements.toml"
                    ));
                }
                Some(decision) => decision.as_decision(),
                None => {
                    return Err(anyhow!(
                        "rules.prefix_rules[{rule_index}] is missing `decision`"
                    ));
                }
            };

            let pattern_tokens = rule
                .pattern
                .iter()
                .enumerate()
                .map(|(token_index, token)| {
                    parse_requirements_pattern_token(rule_index, token_index, token)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            let (first_token, remaining_tokens) =
                pattern_tokens.split_first().ok_or_else(|| {
                    anyhow!("rules.prefix_rules[{rule_index}].pattern cannot be empty")
                })?;
            let rest: Arc<[PatternToken]> = remaining_tokens.to_vec().into();

            for head in first_token.alternatives() {
                policy.add_rule(PrefixRule {
                    pattern: PrefixPattern {
                        first: Arc::from(head.as_str()),
                        rest: rest.clone(),
                    },
                    decision,
                    justification: rule.justification.clone(),
                });
            }
        }
        Ok(policy)
    }
}

fn parse_requirements_pattern_token(
    rule_index: usize,
    token_index: usize,
    token: &RequirementsPatternTokenToml,
) -> anyhow::Result<PatternToken> {
    match (&token.token, &token.any_of) {
        (Some(single), None) => {
            if single.trim().is_empty() {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern[{token_index}].token cannot be empty"
                ));
            }
            Ok(PatternToken::Single(single.clone()))
        }
        (None, Some(alternatives)) => {
            if alternatives.is_empty() {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern[{token_index}].any_of cannot be empty"
                ));
            }
            if alternatives.iter().any(|alternative| alternative.trim().is_empty()) {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern[{token_index}].any_of cannot include empty tokens"
                ));
            }
            Ok(PatternToken::Alts(alternatives.clone()))
        }
        (Some(_), Some(_)) => Err(anyhow!(
            "rules.prefix_rules[{rule_index}].pattern[{token_index}] must set either token or any_of, not both"
        )),
        (None, None) => Err(anyhow!(
            "rules.prefix_rules[{rule_index}].pattern[{token_index}] must set token or any_of"
        )),
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
    fn requirements_toml_converts_to_policy() -> anyhow::Result<()> {
        let requirements: RequirementsToml = toml::from_str(
            r#"
[rules]
[[rules.prefix_rules]]
pattern = [{ token = "rm" }, { token = "-rf" }]
decision = "forbidden"
justification = "destructive"
"#,
        )?;
        let policy = requirements.to_policy()?;
        let result = policy.check_command(
            &["rm".to_string(), "-rf".to_string(), "/tmp".to_string()],
            &MatchOptions::default(),
        );
        assert_eq!(result.decision, Some(ExecDecision::Forbidden));
        Ok(())
    }

    #[test]
    fn requirements_disallow_allow_decision() -> anyhow::Result<()> {
        let requirements: RequirementsToml = toml::from_str(
            r#"
[rules]
[[rules.prefix_rules]]
pattern = [{ token = "echo" }]
decision = "allow"
"#,
        )?;
        assert!(requirements.to_policy().is_err());
        Ok(())
    }
}
