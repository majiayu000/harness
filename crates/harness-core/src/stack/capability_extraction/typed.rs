use super::static_patterns::classify_command_tokens;
use super::{
    declared_raw, inferred_raw, parse_capability, push_unique,
    AgentStackCapabilityExtractionConfidence, AgentStackCapabilityExtractionFailure,
    AgentStackCapabilityExtractionFailureKind, RawCapability,
};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::Path;

pub(super) fn extract_typed(
    component: &AgentStackComponent,
    locator: &str,
    text: &str,
    raw: &mut Vec<RawCapability>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    if component.kind() == AgentStackComponentKind::Hook {
        extract_hook_metadata(component, locator, text, raw, failures);
        return;
    }

    match file_format(locator) {
        Some(FileFormat::Json) => match serde_json::from_str::<Value>(text) {
            Ok(value) => {
                collect_explicit_capabilities(
                    component,
                    &value,
                    "json",
                    rule_id_for_explicit(component.kind(), FileFormat::Json),
                    raw,
                    failures,
                );
                if component.kind() == AgentStackComponentKind::McpServer {
                    collect_mcp_schema_capabilities(&value, raw);
                }
            }
            Err(_) => failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::ParseFailed,
                Some("typed.json_parse"),
                format!("{locator} is not valid JSON"),
            )),
        },
        Some(FileFormat::Toml) => match toml::from_str::<toml::Value>(text) {
            Ok(value) => {
                if let Ok(value) = serde_json::to_value(value) {
                    collect_explicit_capabilities(
                        component,
                        &value,
                        "toml",
                        rule_id_for_explicit(component.kind(), FileFormat::Toml),
                        raw,
                        failures,
                    );
                    collect_policy_prefix_rules(&value, raw);
                }
            }
            Err(_) => failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::ParseFailed,
                Some("typed.toml_parse"),
                format!("{locator} is not valid TOML"),
            )),
        },
        Some(FileFormat::Yaml) => match serde_yaml::from_str::<Value>(text) {
            Ok(value) => collect_explicit_capabilities(
                component,
                &value,
                "yaml",
                rule_id_for_explicit(component.kind(), FileFormat::Yaml),
                raw,
                failures,
            ),
            Err(_) => failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::ParseFailed,
                Some("typed.yaml_parse"),
                format!("{locator} is not valid YAML"),
            )),
        },
        Some(FileFormat::Markdown) => {
            if let Some(front_matter) = yaml_front_matter(text) {
                match serde_yaml::from_str::<Value>(front_matter) {
                    Ok(value) => collect_explicit_capabilities(
                        component,
                        &value,
                        "front_matter",
                        "policy.explicit_capabilities",
                        raw,
                        failures,
                    ),
                    Err(_) => failures.push(AgentStackCapabilityExtractionFailure::new(
                        component,
                        AgentStackCapabilityExtractionFailureKind::ParseFailed,
                        Some("typed.front_matter_parse"),
                        format!("{locator} front matter is not valid YAML"),
                    )),
                }
            }
        }
        None => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFormat {
    Json,
    Toml,
    Yaml,
    Markdown,
}

fn file_format(locator: &str) -> Option<FileFormat> {
    let path = Path::new(locator);
    match path.extension().and_then(OsStr::to_str) {
        Some("json") | Some("json5") => Some(FileFormat::Json),
        Some("toml") => Some(FileFormat::Toml),
        Some("yaml") | Some("yml") => Some(FileFormat::Yaml),
        Some("md") | Some("mdc") => Some(FileFormat::Markdown),
        _ => None,
    }
}

fn rule_id_for_explicit(kind: AgentStackComponentKind, format: FileFormat) -> &'static str {
    match (kind, format) {
        (AgentStackComponentKind::McpServer, _) => "mcp.explicit_capabilities",
        (AgentStackComponentKind::Hook, _) => "hook.metadata_capabilities",
        (AgentStackComponentKind::Policy, _) => "policy.explicit_capabilities",
        _ => "config.explicit_capabilities",
    }
}

fn collect_explicit_capabilities(
    component: &AgentStackComponent,
    value: &Value,
    path: &str,
    rule_id: &'static str,
    raw: &mut Vec<RawCapability>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                if is_capability_key(key) {
                    push_declared_capability_values(
                        component,
                        child,
                        &child_path,
                        rule_id,
                        raw,
                        failures,
                    );
                } else {
                    collect_explicit_capabilities(
                        component,
                        child,
                        &child_path,
                        rule_id,
                        raw,
                        failures,
                    );
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_explicit_capabilities(
                    component,
                    child,
                    &format!("{path}[{index}]"),
                    rule_id,
                    raw,
                    failures,
                );
            }
        }
        _ => {}
    }
}

fn push_declared_capability_values(
    component: &AgentStackComponent,
    value: &Value,
    path: &str,
    rule_id: &'static str,
    raw: &mut Vec<RawCapability>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    let Some(names) = capability_names(value) else {
        return;
    };
    for name in names {
        match parse_capability(&name) {
            Some(capability) => raw.push(declared_raw(
                capability,
                rule_id,
                format!(
                    "{} explicitly declares {name}",
                    component.source().locator().as_str()
                ),
            )),
            None => failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
                Some(rule_id),
                format!("{path} contains unsupported capability `{name}`"),
            )),
        }
    }
}

fn capability_names(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(value) => Some(split_capability_names(value)),
        Value::Array(values) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .flat_map(split_capability_names)
                .collect(),
        ),
        _ => None,
    }
}

fn split_capability_names(value: &str) -> Vec<String> {
    value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_capability_key(key: &str) -> bool {
    matches!(
        key,
        "capabilities" | "harness_capabilities" | "x-harness-capabilities"
    )
}

fn collect_mcp_schema_capabilities(value: &Value, raw: &mut Vec<RawCapability>) {
    let mut seen = std::collections::BTreeSet::new();
    collect_mcp_schema_capabilities_inner(value, raw, &mut seen);
}

fn collect_mcp_schema_capabilities_inner(
    value: &Value,
    raw: &mut Vec<RawCapability>,
    seen: &mut std::collections::BTreeSet<&'static str>,
) {
    match value {
        Value::Object(map) => {
            for key in ["inputSchema", "input_schema"] {
                if let Some(schema) = map.get(key) {
                    infer_schema_capabilities(schema, raw, seen);
                }
            }
            for child in map.values() {
                collect_mcp_schema_capabilities_inner(child, raw, seen);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_mcp_schema_capabilities_inner(child, raw, seen);
            }
        }
        _ => {}
    }
}

fn infer_schema_capabilities(
    schema: &Value,
    raw: &mut Vec<RawCapability>,
    seen: &mut std::collections::BTreeSet<&'static str>,
) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        for capability in capabilities_for_schema_field(name, property) {
            if seen.insert(capability.as_str()) {
                raw.push(inferred_raw(
                    capability,
                    "mcp.input_schema",
                    format!(
                        "MCP input schema field `{name}` indicates {}",
                        capability.as_str()
                    ),
                    AgentStackCapabilityExtractionConfidence::Medium,
                ));
            }
        }
    }
}

fn capabilities_for_schema_field(name: &str, property: &Value) -> Vec<AgentStackCapability> {
    let mut capabilities = Vec::new();
    let normalized = name.to_ascii_lowercase();
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &["command", "cmd", "shell", "script", "argv", "args"],
        AgentStackCapability::Shell,
    );
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &["path", "file", "filename", "output", "write"],
        AgentStackCapability::FileWrite,
    );
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &["url", "uri", "endpoint", "host", "repo", "repository"],
        AgentStackCapability::Network,
    );
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &[
            "token",
            "secret",
            "api_key",
            "apikey",
            "password",
            "credential",
        ],
        AgentStackCapability::SecretRead,
    );
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &["delete", "remove", "overwrite", "force"],
        AgentStackCapability::Destructive,
    );
    push_if_schema_name_matches(
        &mut capabilities,
        &normalized,
        &["production", "deploy", "cluster", "namespace"],
        AgentStackCapability::ProductionWrite,
    );
    if property.get("format").and_then(Value::as_str) == Some("uri") {
        push_unique(&mut capabilities, AgentStackCapability::Network);
    }
    capabilities
}

fn push_if_schema_name_matches(
    capabilities: &mut Vec<AgentStackCapability>,
    name: &str,
    needles: &[&str],
    capability: AgentStackCapability,
) {
    if needles.iter().any(|needle| name.contains(needle)) {
        push_unique(capabilities, capability);
    }
}

fn collect_policy_prefix_rules(value: &Value, raw: &mut Vec<RawCapability>) {
    let Some(rules) = value
        .get("rules")
        .and_then(|rules| rules.get("prefix_rules"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let mut seen = std::collections::BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        let tokens = rule.get("pattern").map(pattern_tokens).unwrap_or_default();
        let decision = rule
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("unspecified");
        for capability in classify_command_tokens(&tokens) {
            if seen.insert(capability.as_str()) {
                raw.push(inferred_raw(
                    capability,
                    "policy.prefix_rule",
                    format!(
                        "policy prefix rule {index} with decision `{decision}` controls a command associated with {}",
                        capability.as_str()
                    ),
                    AgentStackCapabilityExtractionConfidence::Medium,
                ));
            }
        }
    }
}

fn pattern_tokens(value: &Value) -> Vec<String> {
    let mut tokens = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(value) => tokens.push(value.clone()),
                    Value::Object(map) => {
                        if let Some(token) = map.get("token").and_then(Value::as_str) {
                            tokens.push(token.to_owned());
                        }
                        if let Some(alternatives) = map.get("any_of").and_then(Value::as_array) {
                            tokens.extend(
                                alternatives
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::String(value) => tokens.push(value.clone()),
        _ => {}
    }
    tokens
}

fn extract_hook_metadata(
    component: &AgentStackComponent,
    locator: &str,
    text: &str,
    raw: &mut Vec<RawCapability>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    let mut reason = None;
    for line in text.lines().take(64) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix('#') else {
            break;
        };
        let comment = comment.trim();
        if let Some(value) = comment.strip_prefix("harness-reason:") {
            reason = Some(value.trim().to_owned());
            continue;
        }
        let Some(value) = comment
            .strip_prefix("harness-capabilities:")
            .or_else(|| comment.strip_prefix("harness-capability:"))
        else {
            continue;
        };
        for name in split_capability_names(value) {
            match parse_capability(&name) {
                Some(capability) => raw.push(declared_raw(
                    capability,
                    "hook.metadata_capabilities",
                    reason
                        .clone()
                        .unwrap_or_else(|| format!("{locator} declares {name} in hook metadata")),
                )),
                None => failures.push(AgentStackCapabilityExtractionFailure::new(
                    component,
                    AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
                    Some("hook.metadata_capabilities"),
                    format!("{locator} hook metadata contains unsupported capability `{name}`"),
                )),
            }
        }
    }
}

fn yaml_front_matter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}
