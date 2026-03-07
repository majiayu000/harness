mod parser;
mod requirements;

pub use parser::ExecPolicyParser;
pub use requirements::RequirementsToml;

use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    Single(String),
    Alts(Vec<String>),
}

impl PatternToken {
    fn matches(&self, token: &str) -> bool {
        match self {
            Self::Single(expected) => expected == token,
            Self::Alts(alternatives) => alternatives.iter().any(|alternative| alternative == token),
        }
    }

    pub(crate) fn alternatives(&self) -> &[String] {
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
            // Host executable path resolution applies only to absolute commands.
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

pub(crate) fn executable_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}
