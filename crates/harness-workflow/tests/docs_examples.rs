//! Doc-lint: the examples in docs/workflow-declarative-definitions.md must
//! parse and pass the real validation paths, so documentation cannot drift
//! from the implemented schema.

use harness_core::config::workflow::{WorkflowActivityPolicy, WorkflowDefinitionPolicy};
use harness_workflow::runtime::{
    build_declarative_definition, parse_external_state_signal, ActivityResult, ActivitySignal,
    PromptContinuationPolicy,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const DOC_RELATIVE_PATH: &str = "../../docs/workflow-declarative-definitions.md";

fn doc_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DOC_RELATIVE_PATH);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

/// Returns the fenced code block that follows `<!-- doc-example:{name} -->`.
fn doc_example(doc: &str, name: &str) -> String {
    let marker = format!("<!-- doc-example:{name} -->");
    let after_marker = doc
        .split_once(&marker)
        .unwrap_or_else(|| panic!("doc example marker `{marker}` not found"))
        .1;
    let fence_start = after_marker
        .find("```")
        .unwrap_or_else(|| panic!("no code fence after marker `{marker}`"));
    let after_fence = &after_marker[fence_start..];
    let body_start = after_fence
        .find('\n')
        .unwrap_or_else(|| panic!("unterminated fence line after marker `{marker}`"))
        + 1;
    let body = &after_fence[body_start..];
    let body_end = body
        .find("```")
        .unwrap_or_else(|| panic!("unterminated code fence after marker `{marker}`"));
    body[..body_end].to_string()
}

#[derive(Deserialize)]
struct DeclarativeDocExample {
    activities: BTreeMap<String, WorkflowActivityPolicy>,
    definition: WorkflowDefinitionPolicy,
}

#[test]
fn declarative_definition_example_passes_structural_validation() {
    let yaml = doc_example(&doc_text(), "declarative-definition");
    let example: DeclarativeDocExample =
        serde_yaml::from_str(&yaml).expect("documented definition example must parse as YAML");

    let compiled = build_declarative_definition(&example.definition, &example.activities)
        .expect("documented definition example must pass structural validation");
    assert_eq!(compiled.policy().id, "docs_review_flow");
}

#[derive(Deserialize)]
struct ContinuationDocExample {
    continuation: PromptContinuationPolicy,
}

#[test]
fn continuation_policy_example_passes_validation() {
    let json = doc_example(&doc_text(), "continuation-policy");
    let example: ContinuationDocExample =
        serde_json::from_str(&json).expect("documented continuation example must parse as JSON");

    example
        .continuation
        .validate()
        .expect("documented continuation policy must pass validation");
    assert!(example.continuation.active_states.contains("In Progress"));
}

#[test]
fn external_state_signal_example_passes_the_signal_contract() {
    let json = doc_example(&doc_text(), "external-state-signal");
    let signal: ActivitySignal =
        serde_json::from_str(&json).expect("documented signal example must parse as JSON");

    let mut result = ActivityResult::succeeded("implement_prompt", "attempt finished");
    result.signals.push(signal);

    let parsed = parse_external_state_signal(&result)
        .expect("documented signal example must satisfy the external_state contract");
    assert_eq!(parsed.state, "In Progress");
}
