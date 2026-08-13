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

pub(super) const MAX_COMPONENT_FINDINGS: usize = 256;
pub(super) const LIMIT_RULE_ID: &str = "extraction.finding_limit";

#[rustfmt::skip]
#[derive(Debug, Clone, Copy)]
pub(super) enum TypedSource { Auto, Requirements, Starlark, MarkdownPolicy }

#[rustfmt::skip]
pub(super) fn extract_typed(component: &AgentStackComponent, locator: &str, text: &str, source_kind: TypedSource, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if component.kind() == AgentStackComponentKind::Hook { extract_hook_metadata(component, locator, text, raw, failures); return; }
    if matches!(source_kind, TypedSource::Starlark) { collect_starlark_prefix_rules(component, locator, text, raw, failures); return; }
    let Some(format) = (match source_kind { TypedSource::Requirements => Some(FileFormat::Toml), TypedSource::MarkdownPolicy => Some(FileFormat::Markdown), TypedSource::Auto => file_format(locator), TypedSource::Starlark => unreachable!() }) else { return; };
    let is_requirements = matches!(source_kind, TypedSource::Requirements);
    let source = match format.source(text) {
        Ok(Some(source)) => source,
        Ok(None) => return,
        Err(()) => { let (_, rule, label) = format.metadata(); push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::ParseFailed, Some(rule), format!("{locator} is not valid {label}")); return; }
    };
    let (root_name, parse_rule_id, label) = format.metadata();
    match format.parse(source) {
        Ok(value) => {
            if is_requirements && !value.get("rules").is_some_and(Value::is_object) {
                push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), "requirements policy must contain a rules table".to_owned());
                return;
            }
            collect_explicit_capabilities(component, &value, root_name, rule_id_for_explicit(component.kind()), true, raw, failures);
            if component.kind() == AgentStackComponentKind::McpServer { collect_mcp_capabilities(component, &value, raw, failures); }
            collect_policy_prefix_rules(component, &value, is_requirements, raw, failures);
        }
        Err(()) => { push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::ParseFailed, Some(parse_rule_id), format!("{locator} is not valid {label}")); }
    }
}

#[derive(Debug, Clone, Copy)]
#[rustfmt::skip]
enum FileFormat { Json, Json5, Toml, Yaml, Markdown }

const MAX_JSON5_DEPTH: usize = 128;
#[derive(Clone, Copy)]
#[rustfmt::skip]
enum Json5State { Code, String(u8), LineComment, BlockComment }

#[rustfmt::skip]
fn validate_json5_structure(text: &str) -> Result<(), ()> {
    let bytes = text.as_bytes(); let mut state = Json5State::Code;
    let mut depth = 0; let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            Json5State::Code => match (byte, bytes.get(index + 1).copied()) {
                (b'/', Some(b'/')) => { state = Json5State::LineComment; index += 1; }
                (b'/', Some(b'*')) => { state = Json5State::BlockComment; index += 1; }
                (b'\'' | b'"', _) => state = Json5State::String(byte),
                (b'{' | b'[', _) => { depth += 1; if depth > MAX_JSON5_DEPTH { return Err(()); } }
                (b'}' | b']', _) => depth = depth.saturating_sub(1),
                _ => {}
            },
            Json5State::String(quote) => { if byte == b'\\' { index += 1; } else if byte == quote { state = Json5State::Code; } }
            Json5State::LineComment
                if matches!(byte, b'\n' | b'\r')
                    || bytes[index..].starts_with(&[0xe2, 0x80, 0xa8])
                    || bytes[index..].starts_with(&[0xe2, 0x80, 0xa9]) =>
            { state = Json5State::Code }
            Json5State::BlockComment if byte == b'*' && bytes.get(index + 1) == Some(&b'/') => { state = Json5State::Code; index += 1; }
            Json5State::LineComment | Json5State::BlockComment => {}
        }
        index += 1;
    }
    match state { Json5State::Code | Json5State::LineComment => Ok(()), Json5State::String(_) | Json5State::BlockComment => Err(()) }
}
#[rustfmt::skip]
impl FileFormat {
    fn source(self, text: &str) -> Result<Option<&str>, ()> { match self { Self::Markdown => yaml_front_matter(text), _ => Ok(Some(text)) } }

    fn parse(self, text: &str) -> Result<Value, ()> {
        match self {
            Self::Json => serde_json::from_str(text).map_err(|_| ()),
            Self::Json5 => validate_json5_structure(text).and_then(|()| json5::from_str(text).map_err(|_| ())),
            Self::Toml => toml::from_str::<toml::Value>(text).map_err(|_| ()).and_then(|value| serde_json::to_value(value).map_err(|_| ())),
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
#[rustfmt::skip]
fn file_format(locator: &str) -> Option<FileFormat> {
    match Path::new(locator).extension().and_then(OsStr::to_str) {
        Some("json") => Some(FileFormat::Json),
        Some("json5") => Some(FileFormat::Json5),
        Some("toml") => Some(FileFormat::Toml),
        Some("yaml") | Some("yml") => Some(FileFormat::Yaml),
        Some("md") | Some("mdc") => Some(FileFormat::Markdown),
        _ => None,
    }
}
#[rustfmt::skip]
fn rule_id_for_explicit(kind: AgentStackComponentKind) -> &'static str {
    match kind {
        AgentStackComponentKind::McpServer => "mcp.explicit_capabilities",
        AgentStackComponentKind::Hook => "hook.metadata_capabilities",
        AgentStackComponentKind::Policy => "policy.explicit_capabilities",
        _ => "config.explicit_capabilities",
    }
}
#[rustfmt::skip]
fn collect_explicit_capabilities(component: &AgentStackComponent, value: &Value, path: &str, rule_id: &'static str, allow_generic: bool, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    if finding_limit_reached(failures) { return; }
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if is_documentation_key(key) {
                    continue;
                }
                let child_path = format!("{path}.{key}");
                if is_capability_key(key, allow_generic) {
                    push_declared_capability_values(component, child, &child_path, rule_id, raw, failures);
                } else {
                    collect_explicit_capabilities(component, child, &child_path, rule_id, matches!(key.as_str(), "agent_stack" | "harness"), raw, failures);
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_explicit_capabilities(component, child, &format!("{path}[{index}]"), rule_id, false, raw, failures);
            }
        }
        _ => {}
    }
}

#[rustfmt::skip]
fn push_declared_capability_values(component: &AgentStackComponent, value: &Value, path: &str, rule_id: &'static str, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    let valid_shape = matches!(value, Value::String(_))
        || matches!(value, Value::Array(values) if !values.is_empty() && values.iter().all(Value::is_string));
    if !valid_shape {
        push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some(rule_id), format!("{path} must contain a non-empty string or string array"));
        return;
    }
    let mut found = false;
    let mut extract = |value: &str| {
        for name in split_capability_names(value) {
            found = true;
            let keep_going = match parse_capability(name) {
                Some(capability) => push_raw(component, raw, failures, declared_raw(capability, rule_id, format!("{} explicitly declares {name}", component.source().locator().as_str()))),
                None => push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some(rule_id), format!("{path} contains unsupported capability `{name}`")),
            };
            if !keep_going {
                return false;
            }
        }
        true
    };
    match value {
        Value::String(value) => {
            extract(value);
        }
        Value::Array(values) => {
            for value in values {
                let Some(value) = value.as_str() else { return };
                if !extract(value) { return; }
            }
        }
        _ => unreachable!(),
    }
    if !found {
        push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some(rule_id), format!("{path} must contain at least one capability"));
    }
}
fn split_capability_names(value: &str) -> impl Iterator<Item = &str> {
    value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

fn is_capability_key(key: &str, allow_generic: bool) -> bool {
    matches!(key, "harness_capabilities" | "x-harness-capabilities")
        || allow_generic && key == "capabilities"
}

#[rustfmt::skip]
fn push_inferred_once(component: &AgentStackComponent, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>, seen: &mut BTreeSet<&'static str>, capability: AgentStackCapability, rule_id: &'static str, reason: String) {
    if seen.insert(capability.as_str()) {
        push_raw(component, raw, failures, inferred_raw(capability, rule_id, reason, AgentStackCapabilityExtractionConfidence::Medium));
    }
}

#[rustfmt::skip]
fn collect_mcp_capabilities(component: &AgentStackComponent, value: &Value, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    visit_mcp(component, value, raw, failures, &mut BTreeSet::new(), &mut BTreeSet::new());
}

#[rustfmt::skip]
const MCP_SERVER_FIELDS: [(&str, AgentStackCapability); 3] = [
    ("command", AgentStackCapability::Shell), ("args", AgentStackCapability::Shell), ("url", AgentStackCapability::Network),
];

#[rustfmt::skip]
fn visit_mcp(component: &AgentStackComponent, value: &Value, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>, schema_seen: &mut BTreeSet<&'static str>, server_seen: &mut BTreeSet<&'static str>) {
    if finding_limit_reached(failures) { return; }
    match value {
        Value::Object(map) => {
            for key in ["inputSchema", "input_schema"] {
                if let Some(schema) = map.get(key) {
                    infer_schema_capabilities(component, schema, raw, failures, schema_seen);
                }
            }
            for key in ["mcpServers", "mcp_servers"] {
                if let Some(servers) = map.get(key).and_then(Value::as_object) {
                    for (name, server) in servers {
                        for (field, capability) in MCP_SERVER_FIELDS {
                            if server.get(field).is_some_and(has_nonempty_value) {
                                push_inferred_once(component, raw, failures, server_seen, capability, "mcp.server_declaration", format!("MCP server `{name}` field `{field}` indicates {}", capability.as_str()));
                            }
                        }
                        if ["headers", "env"].iter().any(|field| server.get(field).is_some_and(has_sensitive_binding)) {
                            push_inferred_once(component, raw, failures, server_seen, AgentStackCapability::SecretRead, "mcp.server_declaration", format!("MCP server `{name}` contains a secret-bearing binding"));
                        }
                    }
                }
            }
            for (key, child) in map {
                if !is_documentation_key(key) {
                    visit_mcp(component, child, raw, failures, schema_seen, server_seen);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                visit_mcp(component, child, raw, failures, schema_seen, server_seen);
            }
        }
        _ => {}
    }
}

#[rustfmt::skip]
fn infer_schema_capabilities(component: &AgentStackComponent, schema: &Value, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>, seen: &mut BTreeSet<&'static str>) {
    if finding_limit_reached(failures) { return; }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            for capability in capabilities_for_schema_field(name, property) {
                push_inferred_once(component, raw, failures, seen, capability, "mcp.input_schema", format!("MCP input schema field `{name}` indicates {}", capability.as_str()));
            }
        }
    }
    match schema {
        Value::Object(map) => {
            for (key, child) in map {
                if !is_documentation_key(key) {
                    infer_schema_capabilities(component, child, raw, failures, seen);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                infer_schema_capabilities(component, child, raw, failures, seen);
            }
        }
        _ => {}
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
        if schema_name_matches(name, needles)
            && !(capability == SecretRead && schema_name_matches(name, &["endpoint"]))
        {
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
            part.eq_ignore_ascii_case(needle)
                && (start == 0
                    || !name[start - 1].is_ascii_alphanumeric()
                    || name[start].is_ascii_uppercase()
                        && (name[start - 1].is_ascii_lowercase()
                            || name[start - 1].is_ascii_uppercase()
                                && name.get(start + 1).is_some_and(u8::is_ascii_lowercase)))
                && (end == name.len()
                    || !name[end].is_ascii_alphanumeric()
                    || name[end].is_ascii_uppercase()
                        && (name[end - 1].is_ascii_lowercase()
                            || name.get(end + 1).is_some_and(u8::is_ascii_lowercase)))
        })
    })
}

#[rustfmt::skip]
fn is_documentation_key(key: &str) -> bool { matches!(key, "description" | "example" | "examples" | "title" | "default") }

#[rustfmt::skip]
fn has_nonempty_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => values.iter().any(has_nonempty_value), Value::Object(values) => values.values().any(has_nonempty_value),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

#[rustfmt::skip]
fn has_sensitive_binding(value: &Value) -> bool {
    const NAMES: &[&str] = &["authorization", "token", "secret", "api_key", "api-key", "apikey", "password", "credential", "cookie", "access_key"];
    match value {
        Value::Object(values) => values.iter().any(|(name, value)| (has_nonempty_value(value) && schema_name_matches(name, NAMES)) || has_sensitive_binding(value)),
        Value::Array(values) => values.iter().any(has_sensitive_binding),
        Value::String(value) => sensitive_reference(value, NAMES),
        _ => false,
    }
}

#[rustfmt::skip]
fn sensitive_reference(value: &str, names: &[&str]) -> bool {
    let bytes = value.as_bytes(); let mut index = 0;
    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'$') {
        index += offset + 1;
        let braced = bytes.get(index) == Some(&b'{'); if braced { index += 1; } let start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') { index += 1; }
        if start < index && (!braced || bytes.get(index) == Some(&b'}')) && schema_name_matches(&value[start..index], names) { return true; }
    }
    false
}

#[rustfmt::skip]
fn collect_policy_prefix_rules(component: &AgentStackComponent, value: &Value, is_requirements: bool, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    let Some(prefix_rules) = value.get("rules").and_then(|rules| rules.get("prefix_rules")) else {
        return;
    };
    let Some(rules) = prefix_rules.as_array() else {
        push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), "rules.prefix_rules must be an array".to_owned());
        return;
    };
    let mut seen = BTreeSet::new();
    for (index, rule) in rules.iter().enumerate() {
        if is_requirements && rule.get("justification").is_some_and(|value| value.as_str().is_none_or(|value| value.trim().is_empty())) {
            if !push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), format!("policy prefix rule {index} has invalid justification")) { return; }
            continue;
        }
        let pattern_value = rule.get("pattern");
        let requirements_pattern = pattern_value.is_some_and(|value| matches!(value, Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_object)));
        let Some(pattern) = pattern_value.and_then(pattern_tokens).filter(|_| !is_requirements || requirements_pattern) else {
            if !push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), format!("policy prefix rule {index} has no valid pattern")) { return; }
            continue;
        };
        let Some(decision) = rule
            .get("decision")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "allow" | "prompt" | "forbidden") && (!is_requirements || *value != "allow"))
        else {
            if !push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), format!("policy prefix rule {index} has no valid decision")) { return; }
            continue;
        };
        for capability in classify_command_pattern(&pattern) {
            push_inferred_once(component, raw, failures, &mut seen, capability, "policy.prefix_rule", format!("policy prefix rule {index} with decision `{decision}` controls a command associated with {}", capability.as_str()));
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

#[rustfmt::skip]
fn collect_starlark_prefix_rules(component: &AgentStackComponent, locator: &str, text: &str, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
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
    ast.statement().visit_expr(|expr| visit_starlark_expr(expr, &mut |expr| {
        let Expr::Call(function, arguments) = &expr.node else {
            return;
        };
        let Expr::Identifier(name) = &function.node else {
            return;
        };
        if name.ident != "prefix_rule" {
            return;
        }
        let argument = |wanted: &str, position: usize| {
            arguments.args.iter().find_map(|argument| match &argument.node {
                Argument::Named(name, value) if name.node == wanted => Some(value), _ => None,
            }).or_else(|| arguments.args.iter().filter_map(|argument| match &argument.node {
                Argument::Positional(value) => Some(value), _ => None,
            }).nth(position))
        };
        let decision = match argument("decision", 1) {
            None => "allow",
            Some(value) => match starlark_string(value).filter(|value| matches!(*value, "allow" | "prompt" | "forbidden")) {
                Some(decision) => decision,
                None => {
                    push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("policy.prefix_rule"), format!("Starlark policy prefix rule {index} has no valid decision"));
                    index += 1;
                    return;
                }
            },
        };
        let Some(pattern) = argument("pattern", 0).and_then(starlark_pattern_tokens) else {
            push_failure(
                component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration,
                Some("policy.prefix_rule"), format!("Starlark policy prefix rule {index} has no literal pattern"),
            );
            index += 1;
            return;
        };
        for capability in classify_command_pattern(&pattern) {
            push_inferred_once(
                component,
                raw,
                failures,
                &mut seen,
                capability,
                "policy.prefix_rule",
                format!("Starlark prefix rule {index} with decision `{decision}` controls a command associated with {}", capability.as_str()),
            );
        }
        index += 1;
    }));
}

#[rustfmt::skip]
fn visit_starlark_expr(expr: &AstExpr, visit: &mut impl FnMut(&AstExpr)) { visit(expr); expr.node.visit_expr(|child| visit_starlark_expr(child, visit)); }

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

#[rustfmt::skip]
fn extract_hook_metadata(component: &AgentStackComponent, locator: &str, text: &str, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
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
        let Some(value) = comment.strip_prefix("harness-capabilities:").or_else(|| comment.strip_prefix("harness-capability:")) else { continue; };
        let mut found = false;
        for name in split_capability_names(value) {
            found = true;
            match parse_capability(name) {
                Some(capability) => if !push_raw(component, raw, failures, declared_raw(capability, "hook.metadata_capabilities", reason.clone().unwrap_or_else(|| format!("{locator} declares {name} in hook metadata")))) { return; },
                None => if !push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("hook.metadata_capabilities"), format!("{locator} hook metadata contains unsupported capability `{name}`")) { return; },
            }
        }
        if !found {
            push_failure(component, raw, failures, AgentStackCapabilityExtractionFailureKind::InvalidDeclaration, Some("hook.metadata_capabilities"), format!("{locator} hook metadata must contain at least one capability"));
        }
    }
}

#[rustfmt::skip]
fn finding_limit_reached(failures: &[AgentStackCapabilityExtractionFailure]) -> bool {
    failures.iter().any(|failure| failure.rule_id() == Some(LIMIT_RULE_ID))
}

#[rustfmt::skip]
fn push_raw(component: &AgentStackComponent, raw: &mut Vec<RawCapability>, failures: &mut Vec<AgentStackCapabilityExtractionFailure>, item: RawCapability) -> bool {
    if raw.len() + failures.len() < MAX_COMPONENT_FINDINGS - 1 {
        raw.push(item);
        true
    } else {
        record_limit(component, raw, failures);
        false
    }
}

#[rustfmt::skip]
fn push_failure(component: &AgentStackComponent, raw: &[RawCapability], failures: &mut Vec<AgentStackCapabilityExtractionFailure>, kind: AgentStackCapabilityExtractionFailureKind, rule_id: Option<&str>, reason: String) -> bool {
    if raw.len() + failures.len() < MAX_COMPONENT_FINDINGS - 1 {
        failures.push(AgentStackCapabilityExtractionFailure::new(component, kind, rule_id, reason));
        true
    } else {
        record_limit(component, raw, failures);
        false
    }
}

#[rustfmt::skip]
pub(super) fn record_limit(component: &AgentStackComponent, raw: &[RawCapability], failures: &mut Vec<AgentStackCapabilityExtractionFailure>) {
    if !finding_limit_reached(failures) {
        failures.push(AgentStackCapabilityExtractionFailure::new(
            component, AgentStackCapabilityExtractionFailureKind::LimitExceeded, Some(LIMIT_RULE_ID),
            format!("{} exceeds the capability extraction finding limit", component.source().locator().as_str()),
        ));
        debug_assert!(raw.len() + failures.len() <= MAX_COMPONENT_FINDINGS);
    }
}

fn yaml_front_matter(text: &str) -> Result<Option<&str>, ()> {
    let rest = match text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None if text.starts_with("---") => return Err(()),
        None => return Ok(None),
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if matches!(line.trim_end_matches(['\r', '\n']), "---" | "...") {
            return Ok(Some(&rest[..offset]));
        }
        offset += line.len();
    }
    Err(())
}
