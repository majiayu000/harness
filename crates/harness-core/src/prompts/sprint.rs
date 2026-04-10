//! Sprint planning prompts and parsers.

use super::helpers::wrap_external_data;
use super::{PromptParts, SprintPlan};

/// Build prompt for the sprint planner agent.
///
/// The agent outputs a dependency graph (DAG), not rounds. The scheduler
/// handles parallelism by filling slots with ready tasks.
pub fn sprint_plan_prompt(issues: &str) -> String {
    let safe_issues = wrap_external_data(issues);
    format!(
        "You are an Engineering Manager planning a sprint.\n\n\
         ## Pending Issues\n\n\
         {safe_issues}\n\n\
         ## Task\n\n\
         For each issue, read its description and check the codebase to understand which \
         files it would touch. Then output a dependency graph:\n\
         - `depends_on`: list issue numbers that MUST complete before this one starts \
         (because they touch the same files or this fix builds on that fix)\n\
         - Empty `depends_on` means the issue can start immediately\n\
         - Higher priority (P0 > P1) issues should have fewer dependencies\n\
         - Mark issues as \"skip\" if already fixed, duplicate, or invalid\n\n\
         SPRINT_PLAN_START\n\
         {{\n\
           \"tasks\": [\n\
             {{\"issue\": 510, \"depends_on\": []}},\n\
             {{\"issue\": 514, \"depends_on\": []}},\n\
             {{\"issue\": 511, \"depends_on\": [510]}},\n\
             {{\"issue\": 515, \"depends_on\": [514]}}\n\
           ],\n\
           \"skip\": [\n\
             {{\"issue\": 452, \"reason\": \"fixed by recent commit\"}}\n\
           ]\n\
         }}\n\
         SPRINT_PLAN_END\n\n\
         Every pending issue must appear in tasks or skip.\n\
         The JSON between markers must be valid and parseable."
    )
}

/// Parse a `SprintPlan` from agent output by extracting JSON between markers.
/// Robust: finds the first `{` and last `}` between markers, ignoring
/// markdown fences, prose, or other wrapper text agents may add.
pub fn parse_sprint_plan(output: &str) -> Option<SprintPlan> {
    let start = output.find("SPRINT_PLAN_START")? + "SPRINT_PLAN_START".len();
    let end = output[start..].find("SPRINT_PLAN_END")? + start;
    let region = &output[start..end];
    // Extract the JSON object: first '{' to last '}'.
    let json_start = region.find('{')?;
    let json_end = region.rfind('}')? + 1;
    serde_json::from_str(&region[json_start..json_end]).ok()
}

/// Build the sprint-contract generation prompt.
///
/// Asks the agent to emit a machine-readable `sprint-contract` block after the
/// Plan phase so the evaluator can verify implementation against concrete criteria.
///
/// The agent must output a fenced ` ```sprint-contract ` YAML block containing:
/// - `goal`: one-sentence delivery statement
/// - `criteria`: list of `{ id, description, kind, pass_condition }` items
///
/// `kind` must be one of `test`, `llm_judge`, or `manual_check`.
pub fn sprint_contract_prompt(triage_output: &str, plan_output: &str) -> PromptParts {
    let safe_triage = wrap_external_data(triage_output);
    let safe_plan = wrap_external_data(plan_output);
    PromptParts {
        static_instructions: format!(
            "You are a QA Architect generating a machine-readable Sprint Contract.\n\n\
             A Tech Lead assessed the issue:\n{safe_triage}\n\n\
             An architect produced an implementation plan:\n{safe_plan}\n\n\
             Based on the above, produce a Sprint Contract that will be used to \
             automatically verify the implementation before it is reviewed.\n\n\
             Rules:\n\
             - Include 2–6 concrete, verifiable criteria\n\
             - Each criterion must have a clear `pass_condition` that an automated \
               evaluator can check without human interaction\n\
             - Prefer `test` kind for anything covered by the test suite\n\
             - Use `llm_judge` for design/behaviour checks not captured by tests\n\
             - Use `manual_check` only as a last resort\n\n\
             Output the contract as a fenced YAML block on the LAST lines of your response:\n\n\
             \\`\\`\\`sprint-contract\n\
             goal: \"<one sentence>\"\n\
             criteria:\n\
               - id: c1\n\
                 description: \"<what must be true>\"\n\
                 kind: test\n\
                 pass_condition: \"<concrete check>\"\n\
             \\`\\`\\`"
        ),
        context: String::new(),
        dynamic_payload: String::new(),
    }
}

/// Build the evaluator prompt for one implement round.
///
/// The evaluator checks whether the implementation satisfies the sprint contract
/// criteria. It outputs an `eval-result` YAML block with outcome `pass`, `partial`,
/// or `fail`.
pub fn evaluator_prompt(contract_yaml: &str, impl_output: &str, round: u32) -> PromptParts {
    let safe_contract = wrap_external_data(contract_yaml);
    let safe_impl = wrap_external_data(impl_output);
    PromptParts {
        static_instructions: format!(
            "You are an Evaluator checking whether round {round} of implementation \
             satisfies the Sprint Contract.\n\n\
             Sprint Contract:\n{safe_contract}\n\n\
             Implementation output / diff summary:\n{safe_impl}\n\n\
             For each criterion in the contract, determine whether it passes or fails \
             based on the implementation evidence provided.\n\n\
             - `test`: check whether the agent reported test results consistent with a pass\n\
             - `llm_judge`: use your judgement to evaluate whether the description is satisfied\n\
             - `manual_check`: always mark as passed (cannot be automated)\n\n\
             Output the result as a fenced YAML block on the LAST lines of your response:\n\n\
             \\`\\`\\`eval-result\n\
             outcome: pass   # or partial / fail\n\
             criteria_ids:   # for pass — list all criterion IDs\n\
               - c1\n\
             # OR for fail/partial:\n\
             # passed: [c1]\n\
             # failed:\n\
             #   - id: c2\n\
             #     reason: \"brief explanation\"\n\
             \\`\\`\\`"
        ),
        context: String::new(),
        dynamic_payload: String::new(),
    }
}
