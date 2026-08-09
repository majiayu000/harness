use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::fmt;
use thiserror::Error;

const SCHEMA_RAW_MAX: usize = 1_048_576;
const SCHEMA_CANONICAL_MAX: usize = 786_432;
const SCHEMA_DEPTH_MAX: usize = 64;
const SCHEMA_NODES_MAX: usize = 65_536;
const SCHEMA_STRINGS_MAX: usize = 524_288;
const SCHEMA_ENTRIES_MAX: usize = 4_096;
const ANNOTATIONS_RAW_MAX: usize = 65_536;
const ANNOTATIONS_CANONICAL_MAX: usize = 49_152;
const ANNOTATIONS_DEPTH_MAX: usize = 32;
const ANNOTATIONS_NODES_MAX: usize = 4_096;
const ANNOTATIONS_STRINGS_MAX: usize = 32_768;
const ANNOTATIONS_ENTRIES_MAX: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSchemaDialect {
    Draft202012,
    Draft07,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpSingleSchemaKeyword {
    Not,
    If,
    Then,
    Else,
    Contains,
    PropertyNames,
    AdditionalProperties,
    Items,
    AdditionalItems,
    ContentSchema,
    UnevaluatedItems,
    UnevaluatedProperties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpContractLimitKind {
    ConfiguredServerStableKeyBytes,
    ToolNameBytes,
    DescriptionBytes,
    AnnotationsRawBytes,
    AnnotationsCanonicalBytes,
    AnnotationsDepth,
    AnnotationsNodes,
    AnnotationsDecodedStringBytes,
    AnnotationsContainerEntries,
    SchemaRawBytes,
    SchemaCanonicalBytes,
    SchemaDepth,
    SchemaNodes,
    SchemaDecodedStringBytes,
    SchemaContainerEntries,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum McpContractError {
    #[error("the MCP contract JSON has invalid syntax or shape")]
    InvalidJson,
    #[error("the MCP contract JSON contains duplicate object key {0:?}")]
    DuplicateObjectKey(String),
    #[error("the MCP contract JSON root must be an object")]
    RootNotObject,
    #[error("the MCP schema dialect is unsupported")]
    UnsupportedSchemaDialect,
    #[error("the {keyword:?} keyword is not a schema under {dialect:?}")]
    MalformedSingleSchemaKeyword {
        dialect: McpSchemaDialect,
        keyword: McpSingleSchemaKeyword,
    },
    #[error("the MCP contract exceeds the {0:?} limit")]
    LimitExceeded(McpContractLimitKind),
}

#[derive(Debug, Clone, PartialEq)]
enum CanonicalNode {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl Serialize for CanonicalNode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw = RawValue::from_string(
            String::from_utf8(self.canonical_bytes()).expect("canonical JSON is UTF-8"),
        )
        .map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    raw: usize,
    canonical: usize,
    depth: usize,
    nodes: usize,
    strings: usize,
    entries: usize,
    kinds: LimitKinds,
}

#[derive(Debug, Clone, Copy)]
struct LimitKinds {
    raw: McpContractLimitKind,
    canonical: McpContractLimitKind,
    depth: McpContractLimitKind,
    nodes: McpContractLimitKind,
    strings: McpContractLimitKind,
    entries: McpContractLimitKind,
}

const SCHEMA_LIMITS: Limits = Limits {
    raw: SCHEMA_RAW_MAX,
    canonical: SCHEMA_CANONICAL_MAX,
    depth: SCHEMA_DEPTH_MAX,
    nodes: SCHEMA_NODES_MAX,
    strings: SCHEMA_STRINGS_MAX,
    entries: SCHEMA_ENTRIES_MAX,
    kinds: LimitKinds {
        raw: McpContractLimitKind::SchemaRawBytes,
        canonical: McpContractLimitKind::SchemaCanonicalBytes,
        depth: McpContractLimitKind::SchemaDepth,
        nodes: McpContractLimitKind::SchemaNodes,
        strings: McpContractLimitKind::SchemaDecodedStringBytes,
        entries: McpContractLimitKind::SchemaContainerEntries,
    },
};

const ANNOTATION_LIMITS: Limits = Limits {
    raw: ANNOTATIONS_RAW_MAX,
    canonical: ANNOTATIONS_CANONICAL_MAX,
    depth: ANNOTATIONS_DEPTH_MAX,
    nodes: ANNOTATIONS_NODES_MAX,
    strings: ANNOTATIONS_STRINGS_MAX,
    entries: ANNOTATIONS_ENTRIES_MAX,
    kinds: LimitKinds {
        raw: McpContractLimitKind::AnnotationsRawBytes,
        canonical: McpContractLimitKind::AnnotationsCanonicalBytes,
        depth: McpContractLimitKind::AnnotationsDepth,
        nodes: McpContractLimitKind::AnnotationsNodes,
        strings: McpContractLimitKind::AnnotationsDecodedStringBytes,
        entries: McpContractLimitKind::AnnotationsContainerEntries,
    },
};

#[derive(Default)]
struct Budget {
    nodes: usize,
    strings: usize,
}

fn parse_raw(value: &[u8], limits: Limits) -> Result<CanonicalNode, McpContractError> {
    if value.len() > limits.raw {
        return Err(McpContractError::LimitExceeded(limits.kinds.raw));
    }
    let raw: &RawValue =
        serde_json::from_slice(value).map_err(|_| McpContractError::InvalidJson)?;
    let mut budget = Budget::default();
    let node = parse_validated_raw(raw, 1, limits, &mut budget)?;
    if node.canonical_len() > limits.canonical {
        return Err(McpContractError::LimitExceeded(limits.kinds.canonical));
    }
    Ok(node)
}

fn parse_validated_raw(
    raw: &RawValue,
    depth: usize,
    limits: Limits,
    budget: &mut Budget,
) -> Result<CanonicalNode, McpContractError> {
    if depth > limits.depth {
        return Err(McpContractError::LimitExceeded(limits.kinds.depth));
    }
    budget.nodes = budget
        .nodes
        .checked_add(1)
        .ok_or(McpContractError::LimitExceeded(limits.kinds.nodes))?;
    if budget.nodes > limits.nodes {
        return Err(McpContractError::LimitExceeded(limits.kinds.nodes));
    }
    let text = raw.get();
    match text.as_bytes().first().copied() {
        Some(b'{') => parse_container(raw, depth, limits, budget),
        Some(b'[') => parse_container(raw, depth, limits, budget),
        Some(b'"') => {
            let value: String =
                serde_json::from_str(text).map_err(|_| McpContractError::InvalidJson)?;
            charge_string(value.len(), limits, budget)?;
            Ok(CanonicalNode::String(value))
        }
        Some(b't') => Ok(CanonicalNode::Bool(true)),
        Some(b'f') => Ok(CanonicalNode::Bool(false)),
        Some(b'n') => Ok(CanonicalNode::Null),
        Some(_) => Ok(CanonicalNode::Number(text.to_owned())),
        None => Err(McpContractError::InvalidJson),
    }
}

fn parse_container(
    raw: &RawValue,
    depth: usize,
    limits: Limits,
    budget: &mut Budget,
) -> Result<CanonicalNode, McpContractError> {
    let mut deserializer = serde_json::Deserializer::from_str(raw.get());
    let node = deserializer
        .deserialize_any(NodeVisitor {
            depth,
            limits,
            budget,
        })
        .map_err(|error| decode_visitor_error(&error.to_string()))?;
    deserializer
        .end()
        .map_err(|_| McpContractError::InvalidJson)?;
    Ok(node)
}

struct NodeVisitor<'a> {
    depth: usize,
    limits: Limits,
    budget: &'a mut Budget,
}

impl<'de> Visitor<'de> for NodeVisitor<'_> {
    type Value = CanonicalNode;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object or array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(raw) = sequence.next_element::<&RawValue>()? {
            if values.len() == self.limits.entries {
                return Err(serde::de::Error::custom(limit_marker(
                    self.limits.kinds.entries,
                )));
            }
            values.push(
                parse_validated_raw(raw, self.depth + 1, self.limits, self.budget)
                    .map_err(|error| serde::de::Error::custom(visitor_error_marker(&error)))?,
            );
        }
        Ok(CanonicalNode::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if entries.len() == self.limits.entries {
                return Err(serde::de::Error::custom(limit_marker(
                    self.limits.kinds.entries,
                )));
            }
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(duplicate_marker(&key)));
            }
            charge_string(key.len(), self.limits, self.budget).map_err(serde::de::Error::custom)?;
            let raw = map.next_value::<&RawValue>()?;
            let value = parse_validated_raw(raw, self.depth + 1, self.limits, self.budget)
                .map_err(|error| serde::de::Error::custom(visitor_error_marker(&error)))?;
            entries.push((key, value));
        }
        Ok(CanonicalNode::Object(entries))
    }
}

fn charge_string(
    bytes: usize,
    limits: Limits,
    budget: &mut Budget,
) -> Result<(), McpContractError> {
    budget.strings = budget
        .strings
        .checked_add(bytes)
        .ok_or(McpContractError::LimitExceeded(limits.kinds.strings))?;
    if budget.strings > limits.strings {
        Err(McpContractError::LimitExceeded(limits.kinds.strings))
    } else {
        Ok(())
    }
}

fn limit_marker(kind: McpContractLimitKind) -> String {
    format!("LIMIT:{kind:?}")
}

fn duplicate_marker(key: &str) -> String {
    format!("DUPLICATE:{}:{key}", key.len())
}

fn visitor_error_marker(error: &McpContractError) -> String {
    match error {
        McpContractError::DuplicateObjectKey(key) => duplicate_marker(key),
        McpContractError::LimitExceeded(kind) => limit_marker(*kind),
        _ => "INVALID_JSON".to_owned(),
    }
}

fn decode_visitor_error(message: &str) -> McpContractError {
    if let Some(value) = message.split("DUPLICATE:").nth(1) {
        if let Some((length, value)) = value.split_once(':') {
            if let Ok(length) = length.parse::<usize>() {
                if value.len() >= length && value.is_char_boundary(length) {
                    return McpContractError::DuplicateObjectKey(value[..length].to_owned());
                }
            }
        }
    }
    for kind in [
        McpContractLimitKind::AnnotationsDepth,
        McpContractLimitKind::AnnotationsNodes,
        McpContractLimitKind::AnnotationsDecodedStringBytes,
        McpContractLimitKind::AnnotationsContainerEntries,
        McpContractLimitKind::SchemaDepth,
        McpContractLimitKind::SchemaNodes,
        McpContractLimitKind::SchemaDecodedStringBytes,
        McpContractLimitKind::SchemaContainerEntries,
    ] {
        if message.contains(&format!("{kind:?}")) {
            return McpContractError::LimitExceeded(kind);
        }
    }
    McpContractError::InvalidJson
}

impl CanonicalNode {
    fn canonical_len(&self) -> usize {
        match self {
            Self::Null | Self::Bool(true) => 4,
            Self::Bool(false) => 5,
            Self::Number(value) => value.len(),
            Self::String(value) => canonical_string_len(value),
            Self::Array(values) => {
                2 + values.len().saturating_sub(1)
                    + values.iter().map(Self::canonical_len).sum::<usize>()
            }
            Self::Object(entries) => {
                2 + entries.len().saturating_sub(1)
                    + entries
                        .iter()
                        .map(|(key, value)| canonical_string_len(key) + 1 + value.canonical_len())
                        .sum::<usize>()
            }
        }
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut output = Vec::new();
        self.write_canonical(&mut output);
        output
    }

    fn write_canonical(&self, output: &mut Vec<u8>) {
        match self {
            Self::Null => output.extend_from_slice(b"null"),
            Self::Bool(true) => output.extend_from_slice(b"true"),
            Self::Bool(false) => output.extend_from_slice(b"false"),
            Self::Number(value) => output.extend_from_slice(value.as_bytes()),
            Self::String(value) => write_string(value, output),
            Self::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    value.write_canonical(output);
                }
                output.push(b']');
            }
            Self::Object(entries) => {
                output.push(b'{');
                let mut entries = entries.iter().collect::<Vec<_>>();
                entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    write_string(key, output);
                    output.push(b':');
                    value.write_canonical(output);
                }
                output.push(b'}');
            }
        }
    }
}
fn canonical_string_len(value: &str) -> usize {
    2 + value
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\u{08}' | '\t' | '\n' | '\u{0c}' | '\r' => 2,
            control if control <= '\u{1f}' => 6,
            scalar => scalar.len_utf8(),
        })
        .sum::<usize>()
}
fn write_string(value: &str, output: &mut Vec<u8>) {
    output.push(b'"');
    for character in value.chars() {
        match character {
            '"' => output.extend_from_slice(br#"\""#),
            '\\' => output.extend_from_slice(br#"\\"#),
            '\u{08}' => output.extend_from_slice(br#"\b"#),
            '\t' => output.extend_from_slice(br#"\t"#),
            '\n' => output.extend_from_slice(br#"\n"#),
            '\u{0c}' => output.extend_from_slice(br#"\f"#),
            '\r' => output.extend_from_slice(br#"\r"#),
            control if control <= '\u{1f}' => {
                let code = control as u8;
                output.extend_from_slice(b"\\u00");
                output.push(hex(code >> 4));
                output.push(hex(code & 0x0f));
            }
            scalar => {
                let mut buffer = [0; 4];
                output.extend_from_slice(scalar.encode_utf8(&mut buffer).as_bytes());
            }
        }
    }
    output.push(b'"');
}

fn hex(value: u8) -> u8 {
    b"0123456789abcdef"[usize::from(value)]
}

fn object_entries(node: &CanonicalNode) -> Result<&[(String, CanonicalNode)], McpContractError> {
    match node {
        CanonicalNode::Object(entries) => Ok(entries),
        _ => Err(McpContractError::RootNotObject),
    }
}

fn select_dialect(node: &CanonicalNode) -> Result<McpSchemaDialect, McpContractError> {
    let declaration = object_entries(node)?
        .iter()
        .find(|(key, _)| key == "$schema")
        .map(|(_, value)| value);
    match declaration {
        None => Ok(McpSchemaDialect::Draft202012),
        Some(CanonicalNode::String(value))
            if value == "https://json-schema.org/draft/2020-12/schema" =>
        {
            Ok(McpSchemaDialect::Draft202012)
        }
        Some(CanonicalNode::String(value))
            if value == "http://json-schema.org/draft-07/schema#" =>
        {
            Ok(McpSchemaDialect::Draft07)
        }
        _ => Err(McpContractError::UnsupportedSchemaDialect),
    }
}

fn canonicalize_schema(
    node: CanonicalNode,
    dialect: McpSchemaDialect,
    root: bool,
) -> Result<CanonicalNode, McpContractError> {
    match node {
        CanonicalNode::Bool(value) if !root => Ok(CanonicalNode::Bool(value)),
        CanonicalNode::Object(entries) => {
            if !root && entries.iter().any(|(key, _)| key == "$schema") {
                return Err(McpContractError::UnsupportedSchemaDialect);
            }
            let mut output = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let value = canonicalize_schema_member(&key, value, dialect)?;
                output.push((key, value));
            }
            output.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            Ok(CanonicalNode::Object(output))
        }
        _ => Err(McpContractError::RootNotObject),
    }
}

fn canonicalize_schema_member(
    key: &str,
    value: CanonicalNode,
    dialect: McpSchemaDialect,
) -> Result<CanonicalNode, McpContractError> {
    if matches!(key, "required" | "type" | "enum") {
        return canonicalize_instance_set(value);
    }
    if matches!(key, "allOf" | "anyOf" | "oneOf") {
        return canonicalize_schema_array(value, dialect, true);
    }
    if matches!(key, "properties" | "patternProperties")
        || (dialect == McpSchemaDialect::Draft202012 && matches!(key, "$defs" | "dependentSchemas"))
        || (dialect == McpSchemaDialect::Draft07 && key == "definitions")
    {
        return canonicalize_schema_map(value, dialect);
    }
    if let Some(keyword) = shared_single_keyword(key) {
        return canonicalize_single(value, dialect, keyword);
    }
    match (dialect, key) {
        (McpSchemaDialect::Draft202012, "dependentRequired") => {
            canonicalize_string_set_map(value, dialect)
        }
        (McpSchemaDialect::Draft202012, "prefixItems") => {
            canonicalize_schema_array(value, dialect, false)
        }
        (McpSchemaDialect::Draft202012, "items") => {
            canonicalize_single(value, dialect, McpSingleSchemaKeyword::Items)
        }
        (McpSchemaDialect::Draft202012, "contentSchema") => {
            canonicalize_single(value, dialect, McpSingleSchemaKeyword::ContentSchema)
        }
        (McpSchemaDialect::Draft202012, "unevaluatedItems") => {
            canonicalize_single(value, dialect, McpSingleSchemaKeyword::UnevaluatedItems)
        }
        (McpSchemaDialect::Draft202012, "unevaluatedProperties") => canonicalize_single(
            value,
            dialect,
            McpSingleSchemaKeyword::UnevaluatedProperties,
        ),
        (McpSchemaDialect::Draft07, "dependencies") => {
            canonicalize_legacy_dependencies(value, dialect)
        }
        (McpSchemaDialect::Draft07, "items") => match value {
            CanonicalNode::Array(_) => canonicalize_schema_array(value, dialect, false),
            _ => canonicalize_single(value, dialect, McpSingleSchemaKeyword::Items),
        },
        (McpSchemaDialect::Draft07, "additionalItems") => {
            canonicalize_single(value, dialect, McpSingleSchemaKeyword::AdditionalItems)
        }
        _ => canonicalize_instance(value),
    }
}

fn shared_single_keyword(key: &str) -> Option<McpSingleSchemaKeyword> {
    Some(match key {
        "not" => McpSingleSchemaKeyword::Not,
        "if" => McpSingleSchemaKeyword::If,
        "then" => McpSingleSchemaKeyword::Then,
        "else" => McpSingleSchemaKeyword::Else,
        "contains" => McpSingleSchemaKeyword::Contains,
        "propertyNames" => McpSingleSchemaKeyword::PropertyNames,
        "additionalProperties" => McpSingleSchemaKeyword::AdditionalProperties,
        _ => return None,
    })
}

fn canonicalize_single(
    value: CanonicalNode,
    dialect: McpSchemaDialect,
    keyword: McpSingleSchemaKeyword,
) -> Result<CanonicalNode, McpContractError> {
    match value {
        CanonicalNode::Object(_) | CanonicalNode::Bool(_) => {
            canonicalize_schema(value, dialect, false)
        }
        _ => Err(McpContractError::MalformedSingleSchemaKeyword { dialect, keyword }),
    }
}

fn canonicalize_schema_map(
    value: CanonicalNode,
    dialect: McpSchemaDialect,
) -> Result<CanonicalNode, McpContractError> {
    let CanonicalNode::Object(entries) = value else {
        return Err(McpContractError::RootNotObject);
    };
    let mut output = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        output.push((
            key,
            canonicalize_single(value, dialect, McpSingleSchemaKeyword::Items)?,
        ));
    }
    output.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    Ok(CanonicalNode::Object(output))
}

fn canonicalize_schema_array(
    value: CanonicalNode,
    dialect: McpSchemaDialect,
    set: bool,
) -> Result<CanonicalNode, McpContractError> {
    let CanonicalNode::Array(values) = value else {
        return Err(McpContractError::RootNotObject);
    };
    let mut values = values
        .into_iter()
        .map(|value| canonicalize_single(value, dialect, McpSingleSchemaKeyword::Items))
        .collect::<Result<Vec<_>, _>>()?;
    if set {
        values.sort_by_key(CanonicalNode::canonical_bytes);
    }
    Ok(CanonicalNode::Array(values))
}

fn canonicalize_instance_set(value: CanonicalNode) -> Result<CanonicalNode, McpContractError> {
    let CanonicalNode::Array(mut values) = value else {
        return canonicalize_instance(value);
    };
    values = values
        .into_iter()
        .map(canonicalize_instance)
        .collect::<Result<_, _>>()?;
    values.sort_by_key(CanonicalNode::canonical_bytes);
    Ok(CanonicalNode::Array(values))
}

fn canonicalize_string_set_map(
    value: CanonicalNode,
    dialect: McpSchemaDialect,
) -> Result<CanonicalNode, McpContractError> {
    let CanonicalNode::Object(entries) = value else {
        return Err(McpContractError::MalformedSingleSchemaKeyword {
            dialect,
            keyword: McpSingleSchemaKeyword::Items,
        });
    };
    let mut output = Vec::new();
    for (key, value) in entries {
        let CanonicalNode::Array(mut values) = value else {
            return Err(McpContractError::MalformedSingleSchemaKeyword {
                dialect,
                keyword: McpSingleSchemaKeyword::Items,
            });
        };
        if values
            .iter()
            .any(|value| !matches!(value, CanonicalNode::String(_)))
        {
            return Err(McpContractError::MalformedSingleSchemaKeyword {
                dialect,
                keyword: McpSingleSchemaKeyword::Items,
            });
        }
        values.sort_by_key(CanonicalNode::canonical_bytes);
        output.push((key, CanonicalNode::Array(values)));
    }
    output.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    Ok(CanonicalNode::Object(output))
}

fn canonicalize_legacy_dependencies(
    value: CanonicalNode,
    dialect: McpSchemaDialect,
) -> Result<CanonicalNode, McpContractError> {
    let CanonicalNode::Object(entries) = value else {
        return Err(McpContractError::RootNotObject);
    };
    let mut output = Vec::new();
    for (key, value) in entries {
        let value = match value {
            CanonicalNode::Object(_) | CanonicalNode::Bool(_) => {
                canonicalize_schema(value, dialect, false)?
            }
            CanonicalNode::Array(mut values)
                if values
                    .iter()
                    .all(|value| matches!(value, CanonicalNode::String(_))) =>
            {
                values.sort_by_key(CanonicalNode::canonical_bytes);
                CanonicalNode::Array(values)
            }
            _ => return Err(McpContractError::RootNotObject),
        };
        output.push((key, value));
    }
    output.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
    Ok(CanonicalNode::Object(output))
}

fn canonicalize_instance(value: CanonicalNode) -> Result<CanonicalNode, McpContractError> {
    Ok(match value {
        CanonicalNode::Array(values) => CanonicalNode::Array(
            values
                .into_iter()
                .map(canonicalize_instance)
                .collect::<Result<_, _>>()?,
        ),
        CanonicalNode::Object(entries) => {
            let mut entries = entries
                .into_iter()
                .map(|(key, value)| Ok((key, canonicalize_instance(value)?)))
                .collect::<Result<Vec<_>, McpContractError>>()?;
            entries.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            CanonicalNode::Object(entries)
        }
        scalar => scalar,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct McpToolSchema {
    dialect: McpSchemaDialect,
    canonical: CanonicalNode,
}

impl McpToolSchema {
    fn parse(value: &[u8]) -> Result<Self, McpContractError> {
        let parsed = parse_raw(value, SCHEMA_LIMITS)?;
        let dialect = select_dialect(&parsed)?;
        let canonical = canonicalize_schema(parsed, dialect, true)?;
        Ok(Self { dialect, canonical })
    }
}

macro_rules! schema_wrapper {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name(McpToolSchema);
        impl $name {
            pub fn from_json_str(value: &str) -> Result<Self, McpContractError> {
                Self::from_json_slice(value.as_bytes())
            }
            pub fn from_json_slice(value: &[u8]) -> Result<Self, McpContractError> {
                McpToolSchema::parse(value).map(Self)
            }
            pub fn dialect(&self) -> McpSchemaDialect {
                self.0.dialect
            }
            pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
                self.0.canonical.canonical_bytes()
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                self.0.canonical.serialize(serializer)
            }
        }
    };
}

schema_wrapper!(McpInputSchema);
schema_wrapper!(McpOutputSchema);

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolAnnotations(CanonicalNode);

impl McpToolAnnotations {
    pub fn from_json_str(value: &str) -> Result<Self, McpContractError> {
        Self::from_json_slice(value.as_bytes())
    }

    pub fn from_json_slice(value: &[u8]) -> Result<Self, McpContractError> {
        let parsed = parse_raw(value, ANNOTATION_LIMITS)?;
        object_entries(&parsed)?;
        let canonical = canonicalize_instance(parsed)?;
        Ok(Self(canonical))
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        self.0.canonical_bytes()
    }
}

impl Serialize for McpToolAnnotations {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

pub(crate) fn canonicalize_typed_json<T: Serialize>(
    value: &T,
) -> Result<Vec<u8>, McpContractError> {
    let encoded = serde_json::to_vec(value).map_err(|_| McpContractError::InvalidJson)?;
    Ok(parse_raw(&encoded, SCHEMA_LIMITS)?.canonical_bytes())
}
