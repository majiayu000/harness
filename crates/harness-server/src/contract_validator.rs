use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, TurnInterceptor},
    AgentRequest, AgentResponse, Decision,
};

/// Minimum prompt length required to allow execution.
const MIN_PROMPT_LEN: usize = 10;

/// Common action verbs that indicate a well-formed task prompt.
const ACTION_VERBS: &[&str] = &[
    "add", "create", "update", "fix", "remove", "delete", "refactor",
    "implement", "write", "build", "change", "move", "rename", "migrate",
    "review", "check", "test", "run", "deploy", "configure", "enable",
    "disable", "improve", "optimize", "extract",
];

/// Validates task contracts before agent execution.
///
/// - Blocks prompts that are empty or too short (< 10 chars).
/// - Warns on prompts that lack an action verb or acceptance criteria.
pub struct ContractValidator;

impl ContractValidator {
    pub fn new() -> Self {
        Self
    }

    fn has_action_verb(prompt: &str) -> bool {
        let lower = prompt.to_lowercase();
        ACTION_VERBS.iter().any(|v| {
            lower.split_whitespace().any(|word| {
                // Strip trailing punctuation before comparing
                let word = word.trim_matches(|c: char| !c.is_alphabetic());
                word == *v
            })
        })
    }

    fn has_acceptance_criteria(prompt: &str) -> bool {
        let lower = prompt.to_lowercase();
        // Look for common acceptance-criteria signals
        lower.contains("should") || lower.contains("must") || lower.contains("expect")
            || lower.contains("verify") || lower.contains("ensure")
            || lower.contains("assert") || lower.contains("confirm")
    }
}

impl Default for ContractValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TurnInterceptor for ContractValidator {
    fn name(&self) -> &str {
        "contract_validator"
    }

    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult {
        let prompt = req.prompt.trim();

        // Block empty or too-short prompts.
        if prompt.len() < MIN_PROMPT_LEN {
            return InterceptResult::block(format!(
                "prompt too short ({} chars); minimum is {MIN_PROMPT_LEN}",
                prompt.len()
            ));
        }

        // Warn when no clear action verb is present.
        if !Self::has_action_verb(prompt) {
            return InterceptResult {
                decision: Decision::Warn,
                reason: Some(
                    "prompt does not contain a recognizable action verb".to_string(),
                ),
                request: None,
            };
        }

        // Warn when no acceptance criteria are specified.
        if !Self::has_acceptance_criteria(prompt) {
            return InterceptResult {
                decision: Decision::Warn,
                reason: Some(
                    "prompt lacks acceptance criteria (should/must/expect/verify/ensure)".to_string(),
                ),
                request: None,
            };
        }

        InterceptResult::pass()
    }

    async fn post_execute(&self, _req: &AgentRequest, _resp: &AgentResponse) {}

    async fn on_error(&self, _req: &AgentRequest, _error: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AgentRequest, Decision};
    use std::path::PathBuf;

    fn make_req(prompt: &str) -> AgentRequest {
        AgentRequest {
            prompt: prompt.to_string(),
            project_root: PathBuf::from("."),
            allowed_tools: vec![],
            model: None,
            max_budget_usd: None,
            context: vec![],
        }
    }

    #[tokio::test]
    async fn blocks_empty_prompt() {
        let v = ContractValidator::new();
        let result = v.pre_execute(&make_req("")).await;
        assert_eq!(result.decision, Decision::Block);
    }

    #[tokio::test]
    async fn blocks_short_prompt() {
        let v = ContractValidator::new();
        let result = v.pre_execute(&make_req("fix it")).await;
        assert_eq!(result.decision, Decision::Block);
    }

    #[tokio::test]
    async fn warns_without_action_verb() {
        let v = ContractValidator::new();
        // Long enough but no action verb
        let result = v.pre_execute(&make_req("the authentication module needs attention")).await;
        assert_eq!(result.decision, Decision::Warn);
    }

    #[tokio::test]
    async fn warns_without_acceptance_criteria() {
        let v = ContractValidator::new();
        let result = v.pre_execute(&make_req("Fix the login redirect bug in auth.rs")).await;
        assert_eq!(result.decision, Decision::Warn);
    }

    #[tokio::test]
    async fn passes_complete_prompt() {
        let v = ContractValidator::new();
        let result = v
            .pre_execute(&make_req(
                "Fix the login redirect bug in auth.rs. Verify that users are sent to /dashboard.",
            ))
            .await;
        assert_eq!(result.decision, Decision::Pass);
    }
}
