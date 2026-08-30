use super::*;

fn contract() -> WorkflowAgentContract {
    WorkflowAgentContract {
        input_schema: "harness.semantic_activity_input.v1".to_string(),
        output_schema: "harness.semantic_verdict.v1".to_string(),
        allowed_outcomes: vec!["small".to_string(), "large".to_string()],
        tools: AgentContractToolPolicy::None,
        mutation: AgentContractMutationPolicy::Forbidden,
        workspace: AgentContractWorkspacePolicy::EphemeralEmpty,
        fresh_context: true,
        max_primary_attempts: 1,
        max_corrections: 1,
    }
}

#[test]
fn load_workflow_config_reads_agent_contract() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
activities:
  classify_scope:
    prompt: Classify only the supplied facts.
    agent_contract:
      input_schema: harness.semantic_activity_input.v1
      output_schema: harness.semantic_verdict.v1
      allowed_outcomes: [small, medium, large, blocked]
      tools: none
      mutation: forbidden
      workspace: ephemeral_empty
      fresh_context: true
---

Body
"#,
    )?;

    let cfg = load_workflow_config(dir.path())?;
    let activity = cfg
        .activities
        .get("classify_scope")
        .expect("activity should parse");
    let contract = activity
        .agent_contract
        .as_ref()
        .expect("agent contract should parse");
    assert_eq!(contract.input_schema, "harness.semantic_activity_input.v1");
    assert_eq!(contract.output_schema, "harness.semantic_verdict.v1");
    assert_eq!(
        contract.allowed_outcomes,
        vec!["small", "medium", "large", "blocked"]
    );
    assert_eq!(contract.tools, AgentContractToolPolicy::None);
    assert_eq!(contract.mutation, AgentContractMutationPolicy::Forbidden);
    assert_eq!(
        contract.workspace,
        AgentContractWorkspacePolicy::EphemeralEmpty
    );
    assert!(contract.fresh_context);
    assert_eq!(contract.max_primary_attempts, 1);
    assert_eq!(contract.max_corrections, 1);
    contract.validate("classify_scope")?;
    Ok(())
}

#[test]
fn agent_contract_rejects_unknown_field() {
    let error = serde_yaml::from_str::<WorkflowAgentContract>(
        r#"
input_schema: harness.semantic_activity_input.v1
output_schema: harness.semantic_verdict.v1
allowed_outcomes: [small]
tools: none
mutation: forbidden
workspace: ephemeral_empty
fresh_context: true
network: full
"#,
    )
    .expect_err("unknown field must fail");
    assert!(error.to_string().contains("network"), "{error}");
}

#[test]
fn agent_contract_rejects_unknown_enum_values() {
    for (field, yaml) in [
        (
            "tools",
            r#"
input_schema: harness.semantic_activity_input.v1
output_schema: harness.semantic_verdict.v1
allowed_outcomes: [small]
tools: read
mutation: forbidden
workspace: ephemeral_empty
fresh_context: true
"#,
        ),
        (
            "mutation",
            r#"
input_schema: harness.semantic_activity_input.v1
output_schema: harness.semantic_verdict.v1
allowed_outcomes: [small]
tools: none
mutation: allowed
workspace: ephemeral_empty
fresh_context: true
"#,
        ),
        (
            "workspace",
            r#"
input_schema: harness.semantic_activity_input.v1
output_schema: harness.semantic_verdict.v1
allowed_outcomes: [small]
tools: none
mutation: forbidden
workspace: repository_checkout
fresh_context: true
"#,
        ),
    ] {
        assert!(
            serde_yaml::from_str::<WorkflowAgentContract>(yaml).is_err(),
            "unsupported {field} value must fail parsing"
        );
    }
}

#[test]
fn agent_contract_validate_rejects_unsupported_schemas() {
    let mut unknown_input = contract();
    unknown_input.input_schema = "harness.unknown_input.v1".to_string();
    let error = unknown_input.validate("classify").expect_err("must fail");
    assert!(error.to_string().contains("input_schema"), "{error}");

    let mut unknown_output = contract();
    unknown_output.output_schema = "harness.unknown_output.v1".to_string();
    let error = unknown_output.validate("classify").expect_err("must fail");
    assert!(error.to_string().contains("output_schema"), "{error}");
}

#[test]
fn agent_contract_validate_rejects_bad_outcomes() {
    let mut empty = contract();
    empty.allowed_outcomes.clear();
    assert!(empty.validate("classify").is_err());

    let mut padded = contract();
    padded.allowed_outcomes = vec![" small".to_string()];
    let error = padded.validate("classify").expect_err("must fail");
    assert!(error.to_string().contains("whitespace"), "{error}");

    let mut internal_whitespace = contract();
    internal_whitespace.allowed_outcomes = vec!["sm all".to_string()];
    assert!(internal_whitespace.validate("classify").is_err());

    let mut duplicate = contract();
    duplicate.allowed_outcomes = vec!["small".to_string(), "small".to_string()];
    let error = duplicate.validate("classify").expect_err("must fail");
    assert!(error.to_string().contains("repeats"), "{error}");
}

#[test]
fn agent_contract_validate_rejects_inherited_context_and_bad_budgets() {
    let mut inherited = contract();
    inherited.fresh_context = false;
    let error = inherited.validate("classify").expect_err("must fail");
    assert!(error.to_string().contains("fresh_context"), "{error}");

    let mut zero_attempts = contract();
    zero_attempts.max_primary_attempts = 0;
    assert!(zero_attempts.validate("classify").is_err());

    let mut too_many_attempts = contract();
    too_many_attempts.max_primary_attempts = AGENT_CONTRACT_MAX_PRIMARY_ATTEMPTS_CEILING + 1;
    assert!(too_many_attempts.validate("classify").is_err());

    let mut too_many_corrections = contract();
    too_many_corrections.max_corrections = AGENT_CONTRACT_MAX_CORRECTIONS_CEILING + 1;
    assert!(too_many_corrections.validate("classify").is_err());
}

#[test]
fn every_registered_schema_id_resolves_to_a_parseable_document() {
    for schema_id in SUPPORTED_AGENT_CONTRACT_INPUT_SCHEMAS {
        let document = agent_contract_input_schema_document(schema_id)
            .unwrap_or_else(|| panic!("registered input schema '{schema_id}' has no document"));
        let parsed: serde_json::Value =
            serde_json::from_str(document).expect("input schema document is valid JSON");
        assert_eq!(parsed["title"], *schema_id);
    }
    for schema_id in SUPPORTED_AGENT_CONTRACT_OUTPUT_SCHEMAS {
        let document = agent_contract_output_schema_document(schema_id)
            .unwrap_or_else(|| panic!("registered output schema '{schema_id}' has no document"));
        let parsed: serde_json::Value =
            serde_json::from_str(document).expect("output schema document is valid JSON");
        assert_eq!(parsed["title"], *schema_id);
    }
    assert!(agent_contract_input_schema_document("harness.unknown.v1").is_none());
    assert!(agent_contract_output_schema_document("harness.unknown.v1").is_none());
}

#[test]
fn canonical_schema_documents_are_the_runtime_validation_source() {
    let valid_input = serde_json::json!({
        "schema": "harness.semantic_activity_input.v1",
        "subject": {"kind": "issue", "identity": "owner/repo#1"},
        "facts": {},
        "provenance": {},
        "contract_hash": "sha256:contract",
    });
    assert!(
        validate_agent_contract_input("harness.semantic_activity_input.v1", &valid_input).is_ok()
    );
    let mut invalid_input = valid_input;
    invalid_input["unexpected"] = serde_json::json!(true);
    assert!(
        validate_agent_contract_input("harness.semantic_activity_input.v1", &invalid_input)
            .is_err()
    );

    let valid_output = serde_json::json!({
        "schema": "harness.semantic_verdict.v1",
        "outcome": "small",
        "rationale": "Bounded change.",
        "evidence_refs": ["/facts/changed_files"],
    });
    assert!(validate_agent_contract_output("harness.semantic_verdict.v1", &valid_output).is_ok());
    let mut invalid_output = valid_output;
    invalid_output["evidence_refs"] = serde_json::json!("not-an-array");
    assert!(
        validate_agent_contract_output("harness.semantic_verdict.v1", &invalid_output).is_err()
    );
}

#[test]
fn canonical_schemas_reject_whitespace_only_semantic_strings() {
    let valid_input = serde_json::json!({
        "schema": "harness.semantic_activity_input.v1",
        "subject": {"kind": "issue", "identity": "owner/repo#1"},
        "facts": {},
        "provenance": {},
        "contract_hash": "sha256:contract",
    });
    for pointer in ["/subject/kind", "/subject/identity", "/contract_hash"] {
        let mut invalid = valid_input.clone();
        *invalid
            .pointer_mut(pointer)
            .expect("fixture pointer must resolve") = serde_json::json!(" \t");
        assert!(
            validate_agent_contract_input("harness.semantic_activity_input.v1", &invalid).is_err(),
            "whitespace-only input at {pointer} must fail"
        );
    }

    for invalid in [
        serde_json::json!({
            "schema": "harness.semantic_verdict.v1",
            "outcome": "small",
            "rationale": " \t",
            "evidence_refs": [],
        }),
        serde_json::json!({
            "schema": "harness.semantic_verdict.v1",
            "outcome": "small",
            "rationale": "bounded",
            "evidence_refs": [" \t"],
        }),
    ] {
        assert!(
            validate_agent_contract_output("harness.semantic_verdict.v1", &invalid).is_err(),
            "whitespace-only output strings must fail: {invalid}"
        );
    }
}
