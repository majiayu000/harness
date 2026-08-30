//! Explicit, opt-in dogfood against the installed Codex CLI and live model.
//!
//! This is ignored in ordinary test runs because it consumes a real model
//! turn and requires local Codex authentication. Run it by exact name when
//! validating a Codex/model combination before enabling contract dispatch.

use super::*;
use harness_agents::codex::CodexAgent;
use harness_core::config::agents::SandboxMode;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn dogfood_contract() -> PinnedJobAgentContract {
    let contract: WorkflowAgentContract = serde_json::from_value(json!({
        "input_schema": "harness.semantic_activity_input.v1",
        "output_schema": "harness.semantic_verdict.v1",
        "allowed_outcomes": ["small", "large"],
        "tools": "none",
        "mutation": "forbidden",
        "workspace": "ephemeral_empty",
        "fresh_context": true,
    }))
    .expect("dogfood contract is valid");
    let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(
        &serde_json::to_value(&contract).expect("serialize dogfood contract"),
    );
    PinnedJobAgentContract {
        contract,
        prompt: concat!(
            "Classify the supplied change as small or large. ",
            "It is small when exactly one documentation file changes. ",
            "Use only the supplied JSON facts, call no tools, and return the structured verdict."
        )
        .to_string(),
        input: json!({
            "schema": "harness.semantic_activity_input.v1",
            "subject": {"kind": "pull_request", "identity": "owner/repo#2020"},
            "facts": {"changed_files": ["docs/agent-contract.md"]},
            "provenance": {"/changed_files": "server"},
            "contract_hash": contract_hash,
        }),
        definition_hash: "sha256:dogfood-definition".to_string(),
    }
}

#[tokio::test]
#[ignore = "requires installed/authenticated Codex CLI and consumes a live GPT-5.6 Sol turn"]
async fn real_codex_gpt_5_6_sol_contract_dogfood() -> anyhow::Result<()> {
    let backend = Arc::new(CodexAgent::new(
        PathBuf::from("codex"),
        SandboxMode::ReadOnly,
    ));
    let pinned = dogfood_contract();
    let attempt = execute_agent_contract_attempt(
        backend,
        &pinned,
        Some("gpt-5.6-sol".to_string()),
        Some("high".to_string()),
        300,
    )
    .await?;

    let violations = contract_violations(&attempt);
    assert!(
        violations.is_empty(),
        "live attempt violations: {violations:?}"
    );
    let verdict =
        parse_contract_verdict(&attempt.output, &pinned.contract).map_err(anyhow::Error::msg)?;
    assert_eq!(verdict.outcome, "small");
    assert!(
        attempt
            .observations
            .reported_models
            .iter()
            .any(|(model, _)| model == "gpt-5.6-sol"),
        "the live attempt must observe the exact requested model identity"
    );
    Ok(())
}
