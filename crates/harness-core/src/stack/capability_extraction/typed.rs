use super::static_patterns::classify_command_tokens;
use super::{
    declared_raw, inferred_raw, parse_capability, push_unique,
    AgentStackCapabilityExtractionConfidence, AgentStackCapabilityExtractionFailure,
    AgentStackCapabilityExtractionFailureKind, RawCapability,
};
use crate::stack::{AgentStackCapability, AgentStackComponent, AgentStackComponentKind};
use serde_json::Value;
use starlark_syntax::syntax::ast::{Argument, AstExpr, AstLiteral, Expr};
use starlark_syntax::syntax::module::AstModuleFields;
use starlark_syntax::syntax::{AstModule, Dialect};
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

    let Some(format) = file_format(locator) else {
        return;
    };
    if format == FileFormat::Starlark {
        collect_starlark_prefix_rules(component, locator, text, raw, failures);
        return;
    }
    let Some(source) = format.source(text) else {
        return;
    };
    match format.parse(source) {
        Ok(value) => {
            collect_explicit_capabilities(
                component,
                &value,
                format.root_name(),
                rule_id_for_explicit(component.kind()),
                raw,
                failures,
            );
            if component.kind() == AgentStackComponentKind::McpServer {
                collect_mcp_capabilities(&value, raw);
            }
            collect_policy_prefix_rules(&value, raw);
        }
        Err(()) => failures.push(AgentStackCapabilityExtractionFailure::new(
            component,
            AgentStackCapabilityExtractionFailureKind::ParseFailed,
            Some(format.parse_rule_id()),
            format!("{locator} is not valid {}", format.label()),
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFormat {
    Json,
    Json5,
    Toml,
    Yaml,
    Markdown,
    Starlark,
}

impl FileFormat {
    fn source(self, text: &str) -> Option<&str> {
        match self {
            Self::Markdown => yaml_front_matter(text),
            _ => Some(text),
        }
    }

    fn parse(self, text: &str) -> Result<Value, ()> {
        match self {
            Self::Json => serde_json::from_str(text).map_err(|_| ()),
            Self::Json5 => json5::from_str(text).map_err(|_| ()),
            Self::Toml => toml::from_str::<toml::Value>(text)
                .map_err(|_| ())
                .and_then(|value| serde_json::to_value(value).map_err(|_| ())),
            Self::Yaml | Self::Markdown => serde_yaml::from_str(text).map_err(|_| ()),
            Self::Starlark => unreachable!(),
        }
    }

    fn root_name(self) -> &'static str {
        match self {
            Self::Markdown => "front_matter",
            Self::Json => "json",
            Self::Json5 => "json5",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Starlark => unreachable!(),
        }
    }

    fn parse_rule_id(self) -> &'static str {
        match self {
            Self::Json => "typed.json_parse",
            Self::Json5 => "typed.json5_parse",
            Self::Toml => "typed.toml_parse",
            Self::Yaml => "typed.yaml_parse",
            Self::Markdown => "typed.front_matter_parse",
            Self::Starlark => "typed.starlark_parse",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Json5 => "JSON5",
            Self::Toml => "TOML",
            Self::Yaml => "YAML",
            Self::Markdown => "YAML front matter",
            Self::Starlark => "Starlark",
        }
    }
}

fn file_format(locator: &str) -> Option<FileFormat> {
    let path = Path::new(locator);
    match path.extension().and_then(OsStr::to_str) {
        Some("json") => Some(FileFormat::Json),
        Some("json5") => Some(FileFormat::Json5),
        Some("toml") => Some(FileFormat::Toml),
        Some("yaml") | Some("yml") => Some(FileFormat::Yaml),
        Some("md") | Some("mdc") => Some(FileFormat::Markdown),
        Some("star") => Some(FileFormat::Starlark),
        _ => None,
    }
}

fn rule_id_for_explicit(kind: AgentStackComponentKind) -> &'static str {
    match kind {
        AgentStackComponentKind::McpServer => "mcp.explicit_capabilities",
        AgentStackComponentKind::Hook => "hook.metadata_capabilities",
        AgentStackComponentKind::Policy => "policy.explicit_capabilities",
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

fn collect_mcp_capabilities(value: &Value, raw: &mut Vec<RawCapability>) {
    let mut seen = std::collections::BTreeSet::new();
    collect_mcp_schema_capabilities_inner(value, raw, &mut seen);
    seen.clear();
    collect_mcp_server_capabilities(value, raw, &mut seen);
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
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
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
            infer_schema_capabilities(property, raw, seen);
        }
    }
    for key in ["items", "prefixItems", "allOf", "anyOf", "oneOf"] {
        match schema.get(key) {
            Some(Value::Array(items)) => {
                for item in items {
                    infer_schema_capabilities(item, raw, seen);
                }
            }
            Some(item) => infer_schema_capabilities(item, raw, seen),
            None => {}
        }
    }
}

fn collect_mcp_server_capabilities(
    value: &Value,
    raw: &mut Vec<RawCapability>,
    seen: &mut std::collections::BTreeSet<&'static str>,
) {
    match value {
        Value::Object(map) => {
            for key in ["mcpServers", "mcp_servers"] {
                if let Some(servers) = map.get(key).and_then(Value::as_object) {
                    for (name, server) in servers {
                        for (field, capability) in [
                            ("command", AgentStackCapability::Shell),
                            ("args", AgentStackCapability::Shell),
                            ("url", AgentStackCapability::Network),
                            ("headers", AgentStackCapability::SecretRead),
                            ("env", AgentStackCapability::SecretRead),
                        ] {
                            if server.get(field).is_some() && seen.insert(capability.as_str()) {
                                raw.push(inferred_raw(
                                    capability,
                                    "mcp.server_declaration",
                                    format!(
                                        "MCP server `{name}` field `{field}` indicates {}",
                                        capability.as_str()
                                    ),
                                    AgentStackCapabilityExtractionConfidence::Medium,
                                ));
                            }
                        }
                    }
                }
            }
            for child in map.values() {
                collect_mcp_server_capabilities(child, raw, seen);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_mcp_server_capabilities(child, raw, seen);
            }
        }
        _ => {}
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

fn collect_starlark_prefix_rules(
    component: &AgentStackComponent,
    locator: &str,
    text: &str,
    raw: &mut Vec<RawCapability>,
    failures: &mut Vec<AgentStackCapabilityExtractionFailure>,
) {
    let dialect = Dialect {
        enable_def: false,
        enable_lambda: false,
        enable_load: false,
        enable_load_reexport: false,
        enable_top_level_stmt: false,
        ..Dialect::Standard
    };
    let ast = match AstModule::parse(locator, text.to_owned(), &dialect) {
        Ok(ast) => ast,
        Err(_) => {
            failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::ParseFailed,
                Some("typed.starlark_parse"),
                format!("{locator} is not valid Starlark"),
            ));
            return;
        }
    };
    let mut index = 0;
    let mut seen = std::collections::BTreeSet::new();
    ast.statement().visit_expr(|expr| {
        let Expr::Call(function, arguments) = &expr.node else {
            return;
        };
        let Expr::Identifier(name) = &function.node else {
            return;
        };
        if name.ident != "prefix_rule" {
            return;
        }
        let argument = |wanted: &str| {
            arguments.args.iter().find_map(|argument| match &argument.node {
                Argument::Named(name, value) if name.node == wanted => Some(value),
                _ => None,
            })
        };
        let decision = argument("decision")
            .and_then(starlark_string)
            .unwrap_or("unspecified");
        let Some(tokens) = argument("pattern").and_then(starlark_pattern_tokens) else {
            failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
                Some("policy.prefix_rule"),
                format!("Starlark policy prefix rule {index} has no literal pattern"),
            ));
            index += 1;
            return;
        };
        for capability in classify_command_tokens(&tokens) {
            if seen.insert(capability.as_str()) {
                raw.push(inferred_raw(
                    capability,
                    "policy.prefix_rule",
                    format!("Starlark prefix rule {index} with decision `{decision}` controls a command associated with {}", capability.as_str()),
                    AgentStackCapabilityExtractionConfidence::Medium,
                ));
            }
        }
        index += 1;
    });
}

fn starlark_string(expr: &AstExpr) -> Option<&str> {
    match &expr.node {
        Expr::Literal(AstLiteral::String(value)) => Some(&value.node),
        _ => None,
    }
}

fn starlark_pattern_tokens(expr: &AstExpr) -> Option<Vec<String>> {
    let Expr::List(items) = &expr.node else {
        return None;
    };
    let mut tokens = Vec::new();
    for item in items {
        match &item.node {
            Expr::Literal(AstLiteral::String(value)) => tokens.push(value.node.clone()),
            Expr::List(alternatives) => tokens.extend(
                alternatives
                    .iter()
                    .filter_map(starlark_string)
                    .map(str::to_owned),
            ),
            _ => return None,
        }
    }
    (!tokens.is_empty()).then_some(tokens)
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
            reason = (!value.trim().is_empty()).then(|| value.trim().to_owned());
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
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}
