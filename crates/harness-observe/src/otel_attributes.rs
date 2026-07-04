use opentelemetry::KeyValue;
use std::collections::HashSet;
use std::sync::OnceLock;

pub const GEN_AI_SYSTEM: &str = "gen_ai.system";
pub const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
pub const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const HARNESS_WORKFLOW_ID: &str = "harness.workflow.id";
pub const HARNESS_ACTIVITY_KIND: &str = "harness.activity.kind";
pub const HARNESS_OUTCOME: &str = "harness.outcome";
pub const HARNESS_COST_USD: &str = "harness.cost_usd";
pub const HARNESS_RUNTIME_JOB_ID: &str = "harness.runtime.job.id";
pub const HARNESS_THREAD_ID: &str = "harness.thread.id";
pub const HARNESS_TURN_ID: &str = "harness.turn.id";
pub const HARNESS_RETRY_ATTEMPT: &str = "harness.retry.attempt";

pub const ATTRIBUTE_ALLOWLIST: &[&str] = &[
    GEN_AI_SYSTEM,
    GEN_AI_REQUEST_MODEL,
    GEN_AI_USAGE_INPUT_TOKENS,
    GEN_AI_USAGE_OUTPUT_TOKENS,
    HARNESS_COST_USD,
    HARNESS_WORKFLOW_ID,
    HARNESS_ACTIVITY_KIND,
    HARNESS_OUTCOME,
    HARNESS_RUNTIME_JOB_ID,
    HARNESS_THREAD_ID,
    HARNESS_TURN_ID,
    HARNESS_RETRY_ATTEMPT,
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenAiTurnAttributes {
    pub system: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub workflow_id: Option<String>,
    pub activity_kind: Option<String>,
    pub outcome: Option<String>,
    pub runtime_job_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub retry_attempt: Option<u64>,
}

pub fn is_allowed_attribute(key: &str) -> bool {
    static ALLOWLIST: OnceLock<HashSet<&'static str>> = OnceLock::new();
    ALLOWLIST
        .get_or_init(|| ATTRIBUTE_ALLOWLIST.iter().copied().collect())
        .contains(key)
}

pub fn gen_ai_turn_attributes(input: GenAiTurnAttributes) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    push_string(&mut attrs, GEN_AI_SYSTEM, input.system);
    push_string(&mut attrs, GEN_AI_REQUEST_MODEL, input.model);
    push_u64(&mut attrs, GEN_AI_USAGE_INPUT_TOKENS, input.input_tokens);
    push_u64(&mut attrs, GEN_AI_USAGE_OUTPUT_TOKENS, input.output_tokens);
    push_f64(&mut attrs, HARNESS_COST_USD, input.cost_usd);
    push_string(&mut attrs, HARNESS_WORKFLOW_ID, input.workflow_id);
    push_string(&mut attrs, HARNESS_ACTIVITY_KIND, input.activity_kind);
    push_string(&mut attrs, HARNESS_OUTCOME, input.outcome);
    push_string(&mut attrs, HARNESS_RUNTIME_JOB_ID, input.runtime_job_id);
    push_string(&mut attrs, HARNESS_THREAD_ID, input.thread_id);
    push_string(&mut attrs, HARNESS_TURN_ID, input.turn_id);
    push_u64(&mut attrs, HARNESS_RETRY_ATTEMPT, input.retry_attempt);
    attrs
}

fn push_string(attrs: &mut Vec<KeyValue>, key: &'static str, value: Option<String>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        attrs.push(KeyValue::new(key, value));
    }
}

fn push_u64(attrs: &mut Vec<KeyValue>, key: &'static str, value: Option<u64>) {
    if let Some(value) = value.and_then(|value| i64::try_from(value).ok()) {
        attrs.push(KeyValue::new(key, value));
    }
}

fn push_f64(attrs: &mut Vec<KeyValue>, key: &'static str, value: Option<f64>) {
    if let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.0) {
        attrs.push(KeyValue::new(key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_attributes_map_genai_and_harness_allowlist() {
        let attrs = gen_ai_turn_attributes(GenAiTurnAttributes {
            system: Some("codex".to_string()),
            model: Some("gpt-5".to_string()),
            input_tokens: Some(123),
            output_tokens: Some(45),
            cost_usd: Some(0.0123),
            workflow_id: Some("wf-1".to_string()),
            activity_kind: Some("implement".to_string()),
            outcome: Some("done".to_string()),
            runtime_job_id: Some("job-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            retry_attempt: Some(2),
        });

        let keys: Vec<_> = attrs.iter().map(|attr| attr.key.as_str()).collect();
        assert_eq!(keys, ATTRIBUTE_ALLOWLIST);
        assert!(keys.iter().all(|key| is_allowed_attribute(key)));
    }

    #[test]
    fn otel_attributes_omit_unknown_values_instead_of_zero_filling() {
        let attrs = gen_ai_turn_attributes(GenAiTurnAttributes {
            system: Some(" ".to_string()),
            model: None,
            input_tokens: None,
            output_tokens: Some(0),
            cost_usd: Some(f64::NAN),
            workflow_id: None,
            activity_kind: None,
            outcome: None,
            runtime_job_id: None,
            thread_id: None,
            turn_id: None,
            retry_attempt: None,
        });

        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].key.as_str(), GEN_AI_USAGE_OUTPUT_TOKENS);
        assert!(!is_allowed_attribute("gen_ai.prompt"));
    }
}
