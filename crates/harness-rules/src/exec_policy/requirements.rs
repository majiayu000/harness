use crate::exec_policy::{ExecDecision, ExecPolicy, PatternToken, PrefixPattern, PrefixRule};
use anyhow::anyhow;
use anyhow::Context;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

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
            let normalized = single.trim();
            if normalized.is_empty() {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern[{token_index}].token cannot be empty"
                ));
            }
            Ok(PatternToken::Single(normalized.to_string()))
        }
        (None, Some(alternatives)) => {
            if alternatives.is_empty() {
                return Err(anyhow!(
                    "rules.prefix_rules[{rule_index}].pattern[{token_index}].any_of cannot be empty"
                ));
            }
            let normalized = alternatives
                .iter()
                .map(|alternative| {
                    let trimmed = alternative.trim();
                    if trimmed.is_empty() {
                        return Err(anyhow!(
                            "rules.prefix_rules[{rule_index}].pattern[{token_index}].any_of cannot include empty tokens"
                        ));
                    }
                    Ok(trimmed.to_string())
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(PatternToken::Alts(normalized))
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
    use crate::exec_policy::MatchOptions;

    #[test]
    fn requirements_empty_prefix_rules_is_allowed() -> anyhow::Result<()> {
        let requirements: RequirementsToml = toml::from_str(
            r#"
[rules]
"#,
        )?;
        let policy = requirements.to_policy()?;
        let result = policy.check_command(&["git".to_string()], &MatchOptions::default());
        assert_eq!(result.decision, None);
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
