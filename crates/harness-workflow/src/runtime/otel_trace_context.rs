use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowOtelTraceContext {
    pub trace_id: String,
    pub root_span_id: String,
}

impl WorkflowOtelTraceContext {
    pub fn new() -> Self {
        Self {
            trace_id: Uuid::new_v4().simple().to_string(),
            root_span_id: Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(16)
                .collect(),
        }
    }

    pub fn has_valid_trace_ids(&self) -> bool {
        is_lower_hex(&self.trace_id, 32) && is_lower_hex(&self.root_span_id, 16)
    }
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value.as_bytes().iter().any(|byte| *byte != b'0')
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_trace_context_generates_valid_w3c_ids() {
        let context = WorkflowOtelTraceContext::new();

        assert!(context.has_valid_trace_ids());
        assert_eq!(context.trace_id.len(), 32);
        assert_eq!(context.root_span_id.len(), 16);
    }

    #[test]
    fn otel_trace_context_rejects_invalid_ids() {
        assert!(!WorkflowOtelTraceContext {
            trace_id: "0".to_string(),
            root_span_id: "5f467fe7bf42676c".to_string(),
        }
        .has_valid_trace_ids());
        assert!(!WorkflowOtelTraceContext {
            trace_id: "58406520a006649127e371903a2de979".to_string(),
            root_span_id: "0000000000000000".to_string(),
        }
        .has_valid_trace_ids());
    }
}
