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
use std::{collections::BTreeSet, ffi::OsStr, path::Path};

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

    if Path::new(locator).extension().and_then(OsStr::to_str) == Some("star") {
        collect_starlark_prefix_rules(component, locator, text, raw, failures);
        return;
    }
    let Some(format) = file_format(locator) else {
        return;
    };
    let Some(source) = format.source(text) else {
        return;
    };
    let (root_name, parse_rule_id, label) = format.metadata();
    match format.parse(source) {
        Ok(value) => {
            collect_explicit_capabilities(
                component,
                &value,
                root_name,
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
            Some(parse_rule_id),
            format!("{locator} is not valid {label}"),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
#[rustfmt::skip]
enum FileFormat { Json, Json5, Toml, Yaml, Markdown }

const MAX_JSON5_DEPTH: usize = 128;

#[derive(Clone, Copy)]
#[rustfmt::skip]
enum Json5State { Code, String(u8), LineComment, BlockComment }

fn validate_json5_structure(text: &str) -> Result<(), ()> {
    let bytes = text.as_bytes();
    let mut state = Json5State::Code;
    let mut depth = 0;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            Json5State::Code => match (byte, bytes.get(index + 1).copied()) {
                (b'/', Some(b'/')) => {
                    state = Json5State::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = Json5State::BlockComment;
                    index += 1;
                }
                (b'\'' | b'"', _) => state = Json5State::String(byte),
                (b'{' | b'[', _) => {
                    depth += 1;
                    if depth > MAX_JSON5_DEPTH {
                        return Err(());
                    }
                }
                (b'}' | b']', _) => depth = depth.saturating_sub(1),
                _ => {}
            },
            Json5State::String(quote) => {
                if byte == b'\\' {
                    index += 1;
                } else if byte == quote {
                    state = Json5State::Code;
                }
            }
            Json5State::LineComment if matches!(byte, b'\n' | b'\r') => state = Json5State::Code,
            Json5State::BlockComment if byte == b'*' && bytes.get(index + 1) == Some(&b'/') => {
                state = Json5State::Code;
                index += 1;
            }
            Json5State::LineComment | Json5State::BlockComment => {}
        }
        index += 1;
    }
    match state {
        Json5State::Code | Json5State::LineComment => Ok(()),
        Json5State::String(_) | Json5State::BlockComment => Err(()),
    }
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
            Self::Json5 => {
                validate_json5_structure(text).and_then(|()| json5::from_str(text).map_err(|_| ()))
            }
            Self::Toml => toml::from_str::<toml::Value>(text)
                .map_err(|_| ())
                .and_then(|value| serde_json::to_value(value).map_err(|_| ())),
            Self::Yaml | Self::Markdown => serde_yaml::from_str(text).map_err(|_| ()),
        }
    }

    #[rustfmt::skip]
    fn metadata(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Json => ("json", "typed.json_parse", "JSON"),
            Self::Json5 => ("json5", "typed.json5_parse", "JSON5"),
            Self::Toml => ("toml", "typed.toml_parse", "TOML"),
            Self::Yaml => ("yaml", "typed.yaml_parse", "YAML"),
            Self::Markdown => ("front_matter", "typed.front_matter_parse", "YAML front matter"),
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

fn push_inferred_once(
    raw: &mut Vec<RawCapability>,
    seen: &mut BTreeSet<&'static str>,
    capability: AgentStackCapability,
    rule_id: &'static str,
    reason: String,
) {
    if seen.insert(capability.as_str()) {
        raw.push(inferred_raw(
            capability,
            rule_id,
            reason,
            AgentStackCapabilityExtractionConfidence::Medium,
        ));
    }
}

fn collect_mcp_capabilities(value: &Value, raw: &mut Vec<RawCapability>) {
    visit_mcp(value, raw, &mut BTreeSet::new(), &mut BTreeSet::new());
}

#[rustfmt::skip]
const MCP_SERVER_FIELDS: [(&str, AgentStackCapability); 5] = [
    ("command", AgentStackCapability::Shell), ("args", AgentStackCapability::Shell), ("url", AgentStackCapability::Network),
    ("headers", AgentStackCapability::SecretRead), ("env", AgentStackCapability::SecretRead),
];

fn visit_mcp(
    value: &Value,
    raw: &mut Vec<RawCapability>,
    schema_seen: &mut BTreeSet<&'static str>,
    server_seen: &mut BTreeSet<&'static str>,
) {
    match value {
        Value::Object(map) => {
            for key in ["inputSchema", "input_schema"] {
                if let Some(schema) = map.get(key) {
                    infer_schema_capabilities(schema, raw, schema_seen);
                }
            }
            for key in ["mcpServers", "mcp_servers"] {
                if let Some(servers) = map.get(key).and_then(Value::as_object) {
                    for (name, server) in servers {
                        for (field, capability) in MCP_SERVER_FIELDS {
                            if server.get(field).is_some_and(has_nonempty_value) {
                                push_inferred_once(
                                    raw,
                                    server_seen,
                                    capability,
                                    "mcp.server_declaration",
                                    format!(
                                        "MCP server `{name}` field `{field}` indicates {}",
                                        capability.as_str()
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            for child in map.values() {
                visit_mcp(child, raw, schema_seen, server_seen);
            }
        }
        Value::Array(items) => {
            for child in items {
                visit_mcp(child, raw, schema_seen, server_seen);
            }
        }
        _ => {}
    }
}

fn infer_schema_capabilities(
    schema: &Value,
    raw: &mut Vec<RawCapability>,
    seen: &mut BTreeSet<&'static str>,
) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            for capability in capabilities_for_schema_field(name, property) {
                push_inferred_once(
                    raw,
                    seen,
                    capability,
                    "mcp.input_schema",
                    format!(
                        "MCP input schema field `{name}` indicates {}",
                        capability.as_str()
                    ),
                );
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

#[rustfmt::skip]
fn capabilities_for_schema_field(name: &str, property: &Value) -> Vec<AgentStackCapability> {
    use AgentStackCapability::{
        Destructive, FileWrite, Network, ProductionWrite, SecretRead, Shell,
    };
    let patterns: &[(&[&str], AgentStackCapability)] = &[
        (&["command", "cmd", "shell", "script", "argv", "args"], Shell),
        (&["path", "file", "filename", "output", "write"], FileWrite),
        (&["url", "uri", "endpoint", "host", "repo", "repository"], Network),
        (&["token", "secret", "api_key", "apikey", "password", "credential"], SecretRead),
        (&["delete", "remove", "overwrite", "force"], Destructive),
        (&["production", "deploy", "cluster", "namespace"], ProductionWrite),
    ];
    let mut capabilities = Vec::new();
    for &(needles, capability) in patterns {
        if schema_name_matches(name, needles) {
            push_unique(&mut capabilities, capability);
        }
    }
    if property.get("format").and_then(Value::as_str) == Some("uri") {
        push_unique(&mut capabilities, Network);
    }
    capabilities
}

fn schema_name_matches(name: &str, needles: &[&str]) -> bool {
    let name = name.as_bytes();
    needles.iter().any(|needle| {
        let needle = needle.as_bytes();
        name.windows(needle.len()).enumerate().any(|(start, part)| {
            let end = start + part.len();
            let boundary = |left: u8, right: u8| {
                !left.is_ascii_alphanumeric()
                    || right.is_ascii_uppercase() && left.is_ascii_lowercase()
            };
            part.eq_ignore_ascii_case(needle)
                && (start == 0 || boundary(name[start - 1], name[start]))
                && (end == name.len() || boundary(name[end - 1], name[end]))
        })
    })
}

fn has_nonempty_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(has_nonempty_value),
        Value::Object(values) => values.values().any(has_nonempty_value),
        Value::Bool(_) | Value::Number(_) => true,
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
    let mut seen = BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        let Some(pattern) = rule.get("pattern").and_then(pattern_tokens) else {
            continue;
        };
        let decision = rule
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("unspecified");
        for capability in classify_command_pattern(&pattern) {
            push_inferred_once(
                raw,
                &mut seen,
                capability,
                "policy.prefix_rule",
                format!("policy prefix rule {index} with decision `{decision}` controls a command associated with {}", capability.as_str()),
            );
        }
    }
}

type CommandPattern = Vec<Vec<String>>;

fn pattern_tokens(value: &Value) -> Option<CommandPattern> {
    match value {
        Value::Array(items) if !items.is_empty() => items.iter().map(pattern_position).collect(),
        Value::String(value) => Some(vec![vec![trimmed_token(value)?]]),
        _ => None,
    }
}

fn pattern_position(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(value) => Some(vec![trimmed_token(value)?]),
        Value::Object(map) => match (map.get("token"), map.get("any_of")) {
            (Some(Value::String(token)), None) => Some(vec![trimmed_token(token)?]),
            (None, Some(Value::Array(alternatives))) if !alternatives.is_empty() => alternatives
                .iter()
                .map(|value| trimmed_token(value.as_str()?))
                .collect(),
            _ => None,
        },
        _ => None,
    }
}

fn trimmed_token(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn classify_command_pattern(pattern: &CommandPattern) -> Vec<AgentStackCapability> {
    let mut capabilities = Vec::new();
    let tail = pattern
        .iter()
        .skip(1)
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    for head in &pattern[0] {
        let tokens = std::iter::once(head.clone())
            .chain(tail.iter().cloned())
            .collect::<Vec<_>>();
        for capability in classify_command_tokens(&tokens) {
            push_unique(&mut capabilities, capability);
        }
    }
    capabilities
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
    let mut seen = BTreeSet::new();
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
        let Some(pattern) = argument("pattern").and_then(starlark_pattern_tokens) else {
            failures.push(AgentStackCapabilityExtractionFailure::new(
                component,
                AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
                Some("policy.prefix_rule"),
                format!("Starlark policy prefix rule {index} has no literal pattern"),
            ));
            index += 1;
            return;
        };
        for capability in classify_command_pattern(&pattern) {
            push_inferred_once(
                raw,
                &mut seen,
                capability,
                "policy.prefix_rule",
                format!("Starlark prefix rule {index} with decision `{decision}` controls a command associated with {}", capability.as_str()),
            );
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

fn starlark_pattern_tokens(expr: &AstExpr) -> Option<CommandPattern> {
    let Expr::List(items) = &expr.node else {
        return None;
    };
    let mut tokens = Vec::new();
    for item in items {
        let position = match &item.node {
            Expr::Literal(AstLiteral::String(value)) => vec![trimmed_token(&value.node)?],
            Expr::List(alternatives) if !alternatives.is_empty() => alternatives
                .iter()
                .map(|value| trimmed_token(starlark_string(value)?))
                .collect::<Option<_>>()?,
            _ => return None,
        };
        tokens.push(position);
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
