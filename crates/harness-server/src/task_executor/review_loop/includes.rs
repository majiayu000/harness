use serde::Deserialize;

include!("pr_state.rs");

const CODEX_REVIEWER_NAME: &str = "chatgpt-codex-connector[bot]";
const CODEX_REVIEW_COMMAND: &str = "@codex";
const CODEX_APPROVAL_SIGNATURE: &str = "no major issues";

include!("signals.rs");
include!("decision.rs");
include!("runtime_feedback.rs");
include!("wait_budget.rs");
