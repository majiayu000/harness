use super::{
    AgentStackComponent, AgentStackComponentError, AgentStackComponentKind, AgentStackFreshness,
    AgentStackObservationClass, AgentStackSelectionState, AgentStackSource, AgentStackSourceScope,
    AgentStackTrustLevel, Sha256Digest,
};
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

pub const AGENT_STACK_FINGERPRINT_SCHEMA_VERSION: &str = "agent-stack-fingerprint/v0.1";
pub const MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION: &str = "mcp-tool-fingerprint/v0.1";

#[derive(Debug, Error)]
pub enum AgentStackFingerprintError {
    #[error("the Agent Stack fingerprint JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Component(#[from] AgentStackComponentError),
    #[error("the fingerprint identity is blank")]
    BlankIdentity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpInputSchema {
    canonical: Value,
}

impl McpInputSchema {
    pub fn from_json_str(value: &str) -> Result<Self, AgentStackFingerprintError> {
        let parsed: Value = serde_json::from_str(value)?;
        Ok(Self::from_json_value(parsed))
    }

    pub fn from_serializable<T: Serialize>(value: &T) -> Result<Self, AgentStackFingerprintError> {
        Ok(Self::from_json_value(serde_json::to_value(value)?))
    }

    fn from_json_value(value: Value) -> Self {
        Self {
            canonical: canonicalize_schema_value(None, value),
        }
    }

    pub fn digest(&self) -> Result<Sha256Digest, AgentStackFingerprintError> {
        digest_canonical_json_value(&self.canonical)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpToolFingerprint {
    component: AgentStackComponent,
    server_identity: String,
    tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema_digest: Sha256Digest,
    digest: Sha256Digest,
}

impl McpToolFingerprint {
    pub fn new(
        server_identity: impl AsRef<str>,
        tool_name: impl AsRef<str>,
        description: Option<&str>,
        input_schema: McpInputSchema,
    ) -> Result<Self, AgentStackFingerprintError> {
        let server_identity = non_blank_identity(server_identity.as_ref())?;
        let tool_name = non_blank_identity(tool_name.as_ref())?;
        let description = description
            .map(normalize_description)
            .filter(|value| !value.is_empty());
        let input_schema_digest = input_schema.digest()?;
        let payload = McpToolFingerprintPayload {
            schema_version: MCP_TOOL_FINGERPRINT_PAYLOAD_VERSION,
            server_identity: &server_identity,
            tool_name: &tool_name,
            description: description.as_deref(),
            input_schema: &input_schema.canonical,
        };
        let digest = digest_canonical_serializable(&payload)?;
        let component =
            runner_observed_mcp_tool_component(&server_identity, &tool_name, digest.clone())?;
        Ok(Self {
            component,
            server_identity,
            tool_name,
            description,
            input_schema_digest,
            digest,
        })
    }

    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }

    pub fn server_identity(&self) -> &str {
        &self.server_identity
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn input_schema_digest(&self) -> &Sha256Digest {
        &self.input_schema_digest
    }

    pub fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

#[derive(Serialize)]
struct McpToolFingerprintPayload<'a> {
    schema_version: &'static str,
    server_identity: &'a str,
    tool_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    input_schema: &'a Value,
}

pub fn runner_observed_agent_runtime_component(
    runtime_kind: &str,
    integrity: Sha256Digest,
) -> Result<AgentStackComponent, AgentStackFingerprintError> {
    let source = AgentStackSource::logical(
        AgentStackSourceScope::Runner,
        "runtime_executable",
        &stable_logical_segment(runtime_kind)?,
    )?;
    Ok(AgentStackComponent::new(
        AgentStackComponentKind::AgentRuntime,
        source,
        AgentStackObservationClass::RunnerObserved,
        AgentStackSelectionState::Observed,
        AgentStackTrustLevel::RunnerObserved,
        AgentStackFreshness::Fresh,
    )?
    .with_integrity(Some(integrity)))
}

pub fn digest_canonical_serializable<T: Serialize>(
    value: &T,
) -> Result<Sha256Digest, AgentStackFingerprintError> {
    let value = serde_json::to_value(value)?;
    digest_canonical_json_value(&canonicalize_json_value(value))
}

fn runner_observed_mcp_tool_component(
    server_identity: &str,
    tool_name: &str,
    integrity: Sha256Digest,
) -> Result<AgentStackComponent, AgentStackFingerprintError> {
    let stable_path = format!(
        "{}/{}",
        stable_logical_segment(server_identity)?,
        stable_logical_segment(tool_name)?
    );
    let source =
        AgentStackSource::logical(AgentStackSourceScope::Runner, "mcp_tool", &stable_path)?;
    Ok(AgentStackComponent::new(
        AgentStackComponentKind::McpTool,
        source,
        AgentStackObservationClass::RunnerObserved,
        AgentStackSelectionState::Observed,
        AgentStackTrustLevel::RunnerObserved,
        AgentStackFreshness::Fresh,
    )?
    .with_integrity(Some(integrity)))
}

fn non_blank_identity(value: &str) -> Result<String, AgentStackFingerprintError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(AgentStackFingerprintError::BlankIdentity)
    } else {
        Ok(trimmed.to_owned())
    }
}

fn normalize_description(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn stable_logical_segment(value: &str) -> Result<String, AgentStackFingerprintError> {
    let value = non_blank_identity(value)?;
    let mut encoded = String::with_capacity("utf8hex_".len() + value.len() * 2);
    encoded.push_str("utf8hex_");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in value.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

fn digest_canonical_json_value(value: &Value) -> Result<Sha256Digest, AgentStackFingerprintError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(Sha256Digest::from_bytes(&bytes))
}

fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(canonicalize_json_value)
                .collect::<Vec<_>>(),
        ),
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json_value(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        scalar => scalar,
    }
}

fn canonicalize_schema_value(parent_key: Option<&str>, value: Value) -> Value {
    match value {
        Value::Array(values) => {
            let mut values = values
                .into_iter()
                .map(|value| canonicalize_schema_value(None, value))
                .collect::<Vec<_>>();
            if parent_key.is_some_and(schema_array_key_is_order_insensitive) {
                values.sort_by_key(canonical_json_sort_key);
            }
            Value::Array(values)
        }
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        let canonical = canonicalize_schema_value(Some(key.as_str()), value);
                        (key, canonical)
                    })
                    .collect::<Map<_, _>>(),
            )
        }
        scalar => scalar,
    }
}

fn schema_array_key_is_order_insensitive(key: &str) -> bool {
    matches!(
        key,
        "allOf" | "anyOf" | "enum" | "oneOf" | "required" | "type"
    )
}

fn canonical_json_sort_key(value: &Value) -> Vec<u8> {
    match serde_json::to_vec(value) {
        Ok(bytes) => bytes,
        Err(error) => format!("serialization_error:{error}").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mcp_tool_digest_ignores_object_required_and_description_reordering_noise() {
        let left = McpInputSchema::from_json_str(
            r#"{
                "type": "object",
                "required": ["prompt", "thread_id"],
                "properties": {
                    "prompt": { "description": "Prompt", "type": "string" },
                    "thread_id": { "type": "string", "description": "Thread" }
                }
            }"#,
        )
        .unwrap();
        let right = McpInputSchema::from_json_str(
            r#"{
                "properties": {
                    "thread_id": { "description": "Thread", "type": "string" },
                    "prompt": { "type": "string", "description": "Prompt" }
                },
                "required": ["thread_id", "prompt"],
                "type": "object"
            }"#,
        )
        .unwrap();

        let first =
            McpToolFingerprint::new("harness", "harness-reply", Some("Continue\n session"), left)
                .unwrap();
        let second =
            McpToolFingerprint::new("harness", "harness-reply", Some("Continue session"), right)
                .unwrap();

        assert_eq!(first.input_schema_digest(), second.input_schema_digest());
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.component().integrity(), Some(first.digest()));
        first.component().validate().unwrap();
    }

    #[test]
    fn mcp_tool_digest_changes_for_behavior_affecting_schema_change() {
        let first = McpInputSchema::from_serializable(&json!({
            "type": "object",
            "properties": { "prompt": { "type": "string" } },
            "required": ["prompt"]
        }))
        .unwrap();
        let second = McpInputSchema::from_serializable(&json!({
            "type": "object",
            "properties": { "prompt": { "type": "number" } },
            "required": ["prompt"]
        }))
        .unwrap();

        let first =
            McpToolFingerprint::new("harness", "harness", Some("Run prompt"), first).unwrap();
        let second =
            McpToolFingerprint::new("harness", "harness", Some("Run prompt"), second).unwrap();

        assert_ne!(first.input_schema_digest(), second.input_schema_digest());
        assert_ne!(first.digest(), second.digest());
    }

    #[test]
    fn runner_observed_runtime_component_uses_stable_encoded_runtime_kind() {
        let digest = Sha256Digest::from_bytes(b"runtime facts");
        let component = runner_observed_agent_runtime_component("codex_jsonrpc", digest).unwrap();

        assert_eq!(component.kind(), AgentStackComponentKind::AgentRuntime);
        assert_eq!(
            component.source().locator().as_str(),
            "runtime_executable/utf8hex_636f6465785f6a736f6e727063"
        );
        component.validate().unwrap();
    }
}
