use harness_core::types::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_total_tokens: Option<u64>,
}

impl UsageMetrics {
    pub fn component_total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub fn total_tokens(&self) -> u64 {
        self.reported_total_tokens
            .unwrap_or(0)
            .max(self.component_total_tokens())
    }

    pub fn from_token_usage(usage: &TokenUsage) -> Self {
        let prompt_and_output = usage.input_tokens.saturating_add(usage.output_tokens);
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.total_tokens.saturating_sub(prompt_and_output),
            cache_creation_input_tokens: 0,
            reported_total_tokens: Some(usage.total_tokens),
        }
    }

    pub fn from_payload(payload: &Value) -> Option<Self> {
        let input_tokens = payload.get("input_tokens").and_then(Value::as_u64)?;
        let output_tokens = payload.get("output_tokens").and_then(Value::as_u64)?;
        Some(Self {
            input_tokens,
            output_tokens,
            cache_read_input_tokens: payload
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_input_tokens: payload
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            reported_total_tokens: payload
                .get("reported_total_tokens")
                .or_else(|| payload.get("total_tokens"))
                .and_then(Value::as_u64),
        })
    }
}

pub fn parse_result_usage_metrics(line: &str) -> Option<UsageMetrics> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "result" {
        return None;
    }
    UsageMetrics::from_payload(value.get("usage")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_result_usage_with_cache_fields() {
        let line = r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
        let usage = parse_result_usage_metrics(line).expect("usage should parse");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cache_read_input_tokens, 4);
        assert_eq!(usage.cache_creation_input_tokens, 2);
        assert_eq!(usage.total_tokens(), 19);
    }

    #[test]
    fn parses_result_usage_without_cache_fields() {
        let line = r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":3}}"#;
        let usage = parse_result_usage_metrics(line).expect("usage should parse");
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.total_tokens(), 13);
    }

    #[test]
    fn parses_zero_token_result_usage() {
        let line = r#"{"type":"result","usage":{"input_tokens":0,"output_tokens":0}}"#;
        let usage = parse_result_usage_metrics(line).expect("usage should parse");
        assert_eq!(usage.total_tokens(), 0);
    }

    #[test]
    fn preserves_reported_total_when_components_are_lower() {
        let line =
            r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":3,"total_tokens":20}}"#;
        let usage = parse_result_usage_metrics(line).expect("usage should parse");
        assert_eq!(usage.reported_total_tokens, Some(20));
        assert_eq!(usage.total_tokens(), 20);
    }

    #[test]
    fn ignores_malformed_json() {
        assert!(parse_result_usage_metrics("{not-json").is_none());
    }
}
