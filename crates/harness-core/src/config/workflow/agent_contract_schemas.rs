//! Canonical JSON Schema documents behind the agent-contract schema registry.
//!
//! Slice A validates that a contract only names ids listed in
//! `SUPPORTED_AGENT_CONTRACT_*_SCHEMAS`; this module supplies the enforceable
//! document for each id so the runtime can hand the exact schema to the
//! backend's structured-output channel and validate the reply against it.
//! An id without a document here must never be added to the registry.

/// `harness.semantic_activity_input.v1` — the input-fact envelope handed to a
/// no-tool semantic activity. Facts and provenance are persisted server data;
/// the activity receives no other channel.
pub const SEMANTIC_ACTIVITY_INPUT_SCHEMA_V1: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "harness.semantic_activity_input.v1",
  "type": "object",
  "additionalProperties": false,
  "required": ["schema", "subject", "facts", "provenance", "contract_hash"],
  "properties": {
    "schema": {"type": "string", "const": "harness.semantic_activity_input.v1"},
    "subject": {
      "type": "object",
      "additionalProperties": false,
      "required": ["kind", "identity"],
      "properties": {
        "kind": {"type": "string", "minLength": 1, "pattern": "\\S"},
        "identity": {"type": "string", "minLength": 1, "pattern": "\\S"}
      }
    },
    "facts": {"type": "object"},
    "provenance": {"type": "object"},
    "contract_hash": {"type": "string", "minLength": 1, "pattern": "\\S"}
  }
}"#;

/// `harness.semantic_verdict.v1` — the structured reply of a no-tool semantic
/// activity. `outcome` is shape-validated here as a token; the server
/// additionally rejects any value outside the pinned contract's
/// `allowed_outcomes`, so this document never needs a per-contract enum.
pub const SEMANTIC_VERDICT_SCHEMA_V1: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "harness.semantic_verdict.v1",
  "type": "object",
  "additionalProperties": false,
  "required": ["schema", "outcome", "rationale", "evidence_refs"],
  "properties": {
    "schema": {"type": "string", "const": "harness.semantic_verdict.v1"},
    "outcome": {"type": "string", "minLength": 1, "pattern": "^\\S+$"},
    "rationale": {"type": "string", "minLength": 1, "pattern": "\\S"},
    "evidence_refs": {
      "type": "array",
      "items": {"type": "string", "minLength": 1, "pattern": "\\S"}
    }
  }
}"#;

/// Returns the canonical JSON Schema document for a registered input schema id.
pub fn agent_contract_input_schema_document(schema_id: &str) -> Option<&'static str> {
    match schema_id {
        "harness.semantic_activity_input.v1" => Some(SEMANTIC_ACTIVITY_INPUT_SCHEMA_V1),
        _ => None,
    }
}

/// Returns the canonical JSON Schema document for a registered output schema id.
pub fn agent_contract_output_schema_document(schema_id: &str) -> Option<&'static str> {
    match schema_id {
        "harness.semantic_verdict.v1" => Some(SEMANTIC_VERDICT_SCHEMA_V1),
        _ => None,
    }
}

/// Validates a pinned semantic-activity input against the same canonical JSON
/// Schema document handed to agent backends.
pub fn validate_agent_contract_input(
    schema_id: &str,
    input: &serde_json::Value,
) -> Result<(), String> {
    validate_schema_document(
        schema_id,
        agent_contract_input_schema_document(schema_id),
        input,
    )
}

/// Validates an agent reply against the same canonical JSON Schema document
/// handed to the backend's structured-output channel.
pub fn validate_agent_contract_output(
    schema_id: &str,
    output: &serde_json::Value,
) -> Result<(), String> {
    validate_schema_document(
        schema_id,
        agent_contract_output_schema_document(schema_id),
        output,
    )
}

fn validate_schema_document(
    schema_id: &str,
    document: Option<&str>,
    instance: &serde_json::Value,
) -> Result<(), String> {
    let document =
        document.ok_or_else(|| format!("schema `{schema_id}` has no canonical schema document"))?;
    let schema: serde_json::Value = serde_json::from_str(document)
        .map_err(|error| format!("schema `{schema_id}` is invalid JSON: {error}"))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| format!("schema `{schema_id}` cannot be compiled: {error}"))?;
    let errors = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "value does not match schema `{schema_id}`: {}",
            errors.join("; ")
        ))
    }
}
