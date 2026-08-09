use super::*;

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

fn input(value: &str) -> Result<McpInputSchema, McpContractError> {
    McpInputSchema::from_json_str(value)
}

fn output(value: &str) -> Result<McpOutputSchema, McpContractError> {
    McpOutputSchema::from_json_str(value)
}

fn same_schema(left: &str, right: &str) -> bool {
    input(left).unwrap().canonical_bytes() == input(right).unwrap().canonical_bytes()
}

#[test]
fn mcp_input_schema_rejects_every_non_object_root() {
    for value in [
        "null",
        "true",
        "false",
        "0",
        "1.0",
        r#""text""#,
        "[]",
        "[{}]",
    ] {
        assert!(
            matches!(input(value), Err(McpContractError::RootNotObject)),
            "accepted {value}"
        );
    }
}

#[test]
fn mcp_output_schema_rejects_malformed_and_every_non_object_root() {
    assert!(matches!(output("{"), Err(McpContractError::InvalidJson)));
    for value in ["null", "true", "false", "0", r#""text""#, "[]"] {
        assert!(
            matches!(output(value), Err(McpContractError::RootNotObject)),
            "accepted {value}"
        );
    }
}

#[test]
fn absent_schema_dialect_defaults_to_draft_2020_12() {
    assert_eq!(
        input("{}").unwrap().dialect(),
        McpSchemaDialect::Draft202012
    );
}

#[test]
fn exact_supported_schema_dialects_round_trip() {
    for (uri, dialect) in [
        (DRAFT_2020_12, McpSchemaDialect::Draft202012),
        (DRAFT_07, McpSchemaDialect::Draft07),
    ] {
        let schema = input(&format!(r#"{{"$schema":"{uri}"}}"#)).unwrap();
        assert_eq!(schema.dialect(), dialect);
        assert!(String::from_utf8(schema.canonical_bytes())
            .unwrap()
            .contains(uri));
    }
}

#[test]
fn unknown_nonstring_and_nested_schema_dialects_fail_typed() {
    for value in [
        r#"{"$schema":"unknown"}"#,
        r#"{"$schema":7}"#,
        r#"{"$schema":null}"#,
        r#"{"not":{"$schema":"https://json-schema.org/draft/2020-12/schema"}}"#,
    ] {
        assert!(matches!(
            input(value),
            Err(McpContractError::UnsupportedSchemaDialect)
        ));
    }
}

#[test]
fn schema_set_locations_reorder_canonically() {
    for (left, right) in [
        (r#"{"required":["b","a"]}"#, r#"{"required":["a","b"]}"#),
        (
            r#"{"type":["null","string"]}"#,
            r#"{"type":["string","null"]}"#,
        ),
        (r#"{"enum":[2,1]}"#, r#"{"enum":[1,2]}"#),
        (
            r#"{"allOf":[{"type":"string"},{"type":"null"}]}"#,
            r#"{"allOf":[{"type":"null"},{"type":"string"}]}"#,
        ),
        (r#"{"anyOf":[false,true]}"#, r#"{"anyOf":[true,false]}"#),
        (
            r#"{"oneOf":[{"const":2},{"const":1}]}"#,
            r#"{"oneOf":[{"const":1},{"const":2}]}"#,
        ),
    ] {
        assert!(same_schema(left, right), "did not canonicalize {left}");
    }
}

#[test]
fn content_schema_traverses_nested_required_and_one_of_as_schema() {
    assert!(same_schema(
        r#"{"contentSchema":{"required":["b","a"],"oneOf":[{"type":"string"},{"type":"null"}]}}"#,
        r#"{"contentSchema":{"required":["a","b"],"oneOf":[{"type":"null"},{"type":"string"}]}}"#,
    ));
}

#[test]
fn content_schema_remains_ordered_instance_data() {
    let left = format!(r#"{{"$schema":"{DRAFT_07}","contentSchema":{{"enum":[1,2]}}}}"#);
    let right = format!(r#"{{"$schema":"{DRAFT_07}","contentSchema":{{"enum":[2,1]}}}}"#);
    assert!(!same_schema(&left, &right));
}

#[test]
fn draft_07_dependencies_schema_and_string_set_forms_are_context_aware() {
    let left = format!(
        r#"{{"$schema":"{DRAFT_07}","dependencies":{{"a":["c","b"],"d":{{"required":["f","e"]}}}}}}"#
    );
    let right = format!(
        r#"{{"$schema":"{DRAFT_07}","dependencies":{{"d":{{"required":["e","f"]}},"a":["b","c"]}}}}"#
    );
    assert!(same_schema(&left, &right));
}

#[test]
fn draft_07_dependencies_reject_invalid_shapes() {
    for dependency in ["1", r#"["a",1]"#] {
        let value = format!(r#"{{"$schema":"{DRAFT_07}","dependencies":{{"a":{dependency}}}}}"#);
        assert!(input(&value).is_err(), "accepted {value}");
    }
    let non_object = format!(r#"{{"$schema":"{DRAFT_07}","dependencies":[]}}"#);
    assert!(input(&non_object).is_err());
}

#[test]
fn draft_2020_12_legacy_keywords_remain_instance_data() {
    let left = format!(r#"{{"$schema":"{DRAFT_2020_12}","dependencies":{{"a":["b","c"]}}}}"#);
    let right = format!(r#"{{"$schema":"{DRAFT_2020_12}","dependencies":{{"a":["c","b"]}}}}"#);
    assert!(!same_schema(&left, &right));
}

#[test]
fn ordered_schema_annotation_and_extension_arrays_remain_sensitive() {
    for key in ["default", "examples", "x-vendor"] {
        let left = format!(r#"{{"{key}":[1,2]}}"#);
        let right = format!(r#"{{"{key}":[2,1]}}"#);
        assert!(!same_schema(&left, &right));
    }
}

#[test]
fn schema_keyword_shaped_annotation_keys_remain_instance_data() {
    let left = McpToolAnnotations::from_json_str(r#"{"required":["b","a"],"not":{"enum":[2,1]}}"#)
        .unwrap();
    let right = McpToolAnnotations::from_json_str(r#"{"required":["a","b"],"not":{"enum":[1,2]}}"#)
        .unwrap();
    assert_ne!(left.canonical_bytes(), right.canonical_bytes());
}

#[test]
fn draft_2020_12_object_items_traverses_nested_schema() {
    assert!(same_schema(
        r#"{"items":{"required":["b","a"]}}"#,
        r#"{"items":{"required":["a","b"]}}"#,
    ));
}

#[test]
fn draft_2020_12_array_items_is_malformed() {
    assert!(matches!(
        input(r#"{"items":[{},{}]}"#),
        Err(McpContractError::MalformedSingleSchemaKeyword {
            dialect: McpSchemaDialect::Draft202012,
            keyword: McpSingleSchemaKeyword::Items,
        })
    ));
}

#[test]
fn draft_07_array_items_preserves_tuple_order() {
    let left = format!(r#"{{"$schema":"{DRAFT_07}","items":[true,false]}}"#);
    let right = format!(r#"{{"$schema":"{DRAFT_07}","items":[false,true]}}"#);
    assert!(!same_schema(&left, &right));
}

#[test]
fn draft_07_additional_items_traverses_schema_context() {
    let left = format!(
        r#"{{"$schema":"{DRAFT_07}","items":[true],"additionalItems":{{"required":["b","a"]}}}}"#
    );
    let right = format!(
        r#"{{"$schema":"{DRAFT_07}","items":[true],"additionalItems":{{"required":["a","b"]}}}}"#
    );
    assert!(same_schema(&left, &right));
}

#[test]
fn draft_07_additional_items_without_tuple_items_traverses_schema_context() {
    let left = format!(r#"{{"$schema":"{DRAFT_07}","additionalItems":{{"required":["b","a"]}}}}"#);
    let right = format!(r#"{{"$schema":"{DRAFT_07}","additionalItems":{{"required":["a","b"]}}}}"#);
    assert!(same_schema(&left, &right));
}

#[test]
fn shared_single_schema_keywords_traverse_closed_dialect_context() {
    for dialect in [DRAFT_2020_12, DRAFT_07] {
        for keyword in [
            "not",
            "if",
            "then",
            "else",
            "contains",
            "propertyNames",
            "additionalProperties",
        ] {
            let left = format!(r#"{{"$schema":"{dialect}","{keyword}":{{"required":["b","a"]}}}}"#);
            let right =
                format!(r#"{{"$schema":"{dialect}","{keyword}":{{"required":["a","b"]}}}}"#);
            assert!(same_schema(&left, &right), "keyword {keyword}");
        }
    }
}

#[test]
fn shared_single_schema_keywords_reject_non_schema_shapes_with_closed_detail() {
    let cases = [
        ("not", McpSingleSchemaKeyword::Not),
        ("if", McpSingleSchemaKeyword::If),
        ("then", McpSingleSchemaKeyword::Then),
        ("else", McpSingleSchemaKeyword::Else),
        ("contains", McpSingleSchemaKeyword::Contains),
        ("propertyNames", McpSingleSchemaKeyword::PropertyNames),
        (
            "additionalProperties",
            McpSingleSchemaKeyword::AdditionalProperties,
        ),
    ];
    for dialect in [
        (DRAFT_2020_12, McpSchemaDialect::Draft202012),
        (DRAFT_07, McpSchemaDialect::Draft07),
    ] {
        for (keyword, expected) in cases {
            let value = format!(r#"{{"$schema":"{}","{keyword}":1}}"#, dialect.0);
            assert!(matches!(
                input(&value),
                Err(McpContractError::MalformedSingleSchemaKeyword {
                    dialect: actual_dialect,
                    keyword: actual_keyword,
                }) if actual_dialect == dialect.1 && actual_keyword == expected
            ));
        }
    }
}

#[test]
fn nested_schema_dialect_is_rejected_in_every_shared_single_schema_keyword() {
    for keyword in [
        "not",
        "if",
        "then",
        "else",
        "contains",
        "propertyNames",
        "additionalProperties",
    ] {
        let value = format!(r#"{{"{keyword}":{{"$schema":"{DRAFT_2020_12}"}}}}"#);
        assert!(matches!(
            input(&value),
            Err(McpContractError::UnsupportedSchemaDialect)
        ));
    }
}

#[test]
fn draft_2020_12_dependent_required_property_arrays_are_canonical_string_sets() {
    assert!(same_schema(
        r#"{"dependentRequired":{"a":["c","b"]}}"#,
        r#"{"dependentRequired":{"a":["b","c"]}}"#,
    ));
}

#[test]
fn dependent_required_rejects_non_string_set_shapes() {
    for value in [
        r#"{"dependentRequired":[]}"#,
        r#"{"dependentRequired":{"a":1}}"#,
        r#"{"dependentRequired":{"a":["b",1]}}"#,
    ] {
        assert!(input(value).is_err(), "accepted {value}");
    }
}

#[test]
fn boolean_items_is_canonical_nested_schema() {
    assert!(input(r#"{"items":true}"#).is_ok());
    let draft_07 = format!(r#"{{"$schema":"{DRAFT_07}","items":false}}"#);
    assert!(input(&draft_07).is_ok());
}

#[test]
fn raw_schema_rejects_duplicate_keys() {
    for (value, expected) in [
        (r#"{"outer":{"same":1,"same":2}}"#, "same"),
        (r#"{"":1,"":2}"#, ""),
        (r#"{" at line":1," at line":2}"#, " at line"),
        (r#"{"工具":1,"工具":2}"#, "工具"),
    ] {
        let result = input(value);
        assert!(
            matches!(
                result,
                Err(McpContractError::DuplicateObjectKey(ref key)) if key == expected
            ),
            "{result:?}"
        );
    }
}

#[test]
fn deep_and_wide_schema_input_fails_typed_without_panicking() {
    let mut deep = "{}".to_owned();
    for _ in 0..65 {
        deep = format!(r#"{{"not":{deep}}}"#);
    }
    assert!(matches!(
        input(&deep),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaDepth
        ))
    ));

    let wide = format!(
        "{{{}}}",
        (0..4_097)
            .map(|index| format!(r#""k{index}":null"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(matches!(
        input(&wide),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaContainerEntries
        ))
    ));
}

fn null_object(entries: usize) -> String {
    format!(
        "{{{}}}",
        (0..entries)
            .map(|index| format!(r#""k{index}":null"#))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn nested_arrays(child_lengths: &[usize]) -> String {
    let children = child_lengths
        .iter()
        .map(|length| format!("[{}]", vec!["null"; *length].join(",")))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"x":[{children}]}}"#)
}

fn annotation_canonical_fixture(extra_key_bytes: usize) -> String {
    let group_lengths = [1_024, 1_024, 1_022];
    let mut global = 0usize;
    let groups = group_lengths
        .iter()
        .enumerate()
        .map(|(group, length)| {
            let entries = (0..*length)
                .map(|_| {
                    let mut key = format!("k{global:07}");
                    if global == 0 {
                        key.push_str(&"x".repeat(13 + extra_key_bytes));
                    }
                    global += 1;
                    format!(r#""{key}":null"#)
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(r#""{}":{{{entries}}}"#, char::from(b'a' + group as u8))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{groups}}}")
}

#[test]
fn mcp_output_schema_applies_every_exact_and_limit_plus_one_bound() {
    let raw_exact = format!("{{}}{}", " ".repeat(1_048_574));
    assert_eq!(raw_exact.len(), 1_048_576);
    assert!(output(&raw_exact).is_ok());
    assert!(matches!(
        output(&format!("{raw_exact} ")),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaRawBytes
        ))
    ));

    let mut depth_exact = "{}".to_owned();
    for _ in 1..64 {
        depth_exact = format!(r#"{{"not":{depth_exact}}}"#);
    }
    assert!(output(&depth_exact).is_ok());
    assert!(matches!(
        output(&format!(r#"{{"not":{depth_exact}}}"#)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaDepth
        ))
    ));

    let mut exact_node_lengths = vec![4_096; 15];
    exact_node_lengths.push(4_078);
    let exact_nodes = nested_arrays(&exact_node_lengths);
    assert!(output(&exact_nodes).is_ok());
    exact_node_lengths[15] += 1;
    assert!(matches!(
        output(&nested_arrays(&exact_node_lengths)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaNodes
        ))
    ));

    let exact_strings = format!(r#"{{"x":"{}"}}"#, "a".repeat(524_287));
    assert!(output(&exact_strings).is_ok());
    assert!(matches!(
        output(&format!(r#"{{"x":"{}"}}"#, "a".repeat(524_288))),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaDecodedStringBytes
        ))
    ));

    assert!(output(&null_object(4_096)).is_ok());
    assert!(matches!(
        output(&null_object(4_097)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaContainerEntries
        ))
    ));

    let canonical_exact = format!(r#"{{"x":1{}}}"#, "0".repeat(786_425));
    assert_eq!(canonical_exact.len(), 786_432);
    assert!(output(&canonical_exact).is_ok());
    let canonical_over = format!(r#"{{"x":1{}}}"#, "0".repeat(786_426));
    assert_eq!(canonical_over.len(), 786_433);
    assert!(matches!(
        output(&canonical_over),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::SchemaCanonicalBytes
        ))
    ));
}

#[test]
fn mcp_annotations_apply_every_exact_and_limit_plus_one_bound() {
    let raw_exact = format!("{{}}{}", " ".repeat(65_534));
    assert_eq!(raw_exact.len(), 65_536);
    assert!(McpToolAnnotations::from_json_str(&raw_exact).is_ok());
    assert!(matches!(
        McpToolAnnotations::from_json_str(&format!("{raw_exact} ")),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsRawBytes
        ))
    ));

    let mut depth_exact = "{}".to_owned();
    for _ in 1..32 {
        depth_exact = format!(r#"{{"x":{depth_exact}}}"#);
    }
    assert!(McpToolAnnotations::from_json_str(&depth_exact).is_ok());
    assert!(matches!(
        McpToolAnnotations::from_json_str(&format!(r#"{{"x":{depth_exact}}}"#)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsDepth
        ))
    ));

    let exact_nodes = nested_arrays(&[1_023, 1_023, 1_022, 1_022]);
    assert!(McpToolAnnotations::from_json_str(&exact_nodes).is_ok());
    let over_nodes = nested_arrays(&[1_023, 1_023, 1_022, 1_023]);
    assert!(matches!(
        McpToolAnnotations::from_json_str(&over_nodes),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsNodes
        ))
    ));

    let exact_strings = format!(r#"{{"x":"{}"}}"#, "a".repeat(32_767));
    assert!(McpToolAnnotations::from_json_str(&exact_strings).is_ok());
    assert!(matches!(
        McpToolAnnotations::from_json_str(&format!(r#"{{"x":"{}"}}"#, "a".repeat(32_768))),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsDecodedStringBytes
        ))
    ));

    assert!(McpToolAnnotations::from_json_str(&null_object(1_024)).is_ok());
    assert!(matches!(
        McpToolAnnotations::from_json_str(&null_object(1_025)),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsContainerEntries
        ))
    ));

    let canonical_exact = annotation_canonical_fixture(0);
    assert_eq!(canonical_exact.len(), 49_152);
    assert!(McpToolAnnotations::from_json_str(&canonical_exact).is_ok());
    let canonical_over = annotation_canonical_fixture(1);
    assert_eq!(canonical_over.len(), 49_153);
    assert!(matches!(
        McpToolAnnotations::from_json_str(&canonical_over),
        Err(McpContractError::LimitExceeded(
            McpContractLimitKind::AnnotationsCanonicalBytes
        ))
    ));
}

fn mcp_constructor(
    tool_name: &str,
    description: Option<&str>,
) -> Result<McpToolFingerprintPayload, AgentStackFingerprintError> {
    let base = AgentStackSource::logical(AgentStackSourceScope::Runner, "configured_mcp", "limits")
        .unwrap();
    McpToolFingerprintPayload::new(
        ConfiguredMcpServerBinding::new(base, "server").unwrap(),
        tool_name,
        description,
        None,
        input("{}").unwrap(),
        None,
    )
}

#[test]
fn mcp_tool_name_and_description_apply_exact_and_limit_plus_one_bounds() {
    assert!(mcp_constructor(&"t".repeat(1_024), None).is_ok());
    assert!(matches!(
        mcp_constructor(&"t".repeat(1_025), None),
        Err(AgentStackFingerprintError::McpContract(
            McpContractError::LimitExceeded(McpContractLimitKind::ToolNameBytes)
        ))
    ));
    assert!(mcp_constructor("tool", Some(&"d".repeat(65_536))).is_ok());
    assert!(matches!(
        mcp_constructor("tool", Some(&"d".repeat(65_537))),
        Err(AgentStackFingerprintError::McpContract(
            McpContractError::LimitExceeded(McpContractLimitKind::DescriptionBytes)
        ))
    ));
}

fn mcp_digest(
    description: Option<&str>,
    annotations: Option<&str>,
    output_schema: Option<&str>,
) -> String {
    AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(
        description,
        annotations,
        "{}",
        output_schema,
    ))
    .unwrap()
    .fingerprint_digest()
    .as_str()
    .to_owned()
}

#[test]
fn mcp_description_preserves_absent_empty_space_tab_and_newline_distinctions() {
    let values = [None, Some(""), Some(" "), Some("\t"), Some("\n")]
        .into_iter()
        .map(|description| mcp_digest(description, None, None))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(values.len(), 5);
}

#[test]
fn mcp_output_schema_absence_and_presence_are_distinct() {
    assert_ne!(
        mcp_digest(None, None, None),
        mcp_digest(None, None, Some("{}"))
    );
}

#[test]
fn mcp_annotations_preserve_absent_empty_hints_title_vendor_values_and_ordered_arrays() {
    let values = [
        None,
        Some("{}"),
        Some(r#"{"readOnlyHint":false}"#),
        Some(r#"{"title":""}"#),
        Some(r#"{"vendor":""}"#),
        Some(r#"{"vendor":[1,2]}"#),
        Some(r#"{"vendor":[2,1]}"#),
    ]
    .into_iter()
    .map(|annotations| mcp_digest(None, annotations, None))
    .collect::<std::collections::HashSet<_>>();
    assert_eq!(values.len(), 7);
}

#[test]
fn mcp_annotation_hints_do_not_infer_capabilities() {
    let envelope = AgentStackFingerprintEnvelope::mcp_tool(mcp_payload(
        None,
        Some(
            r#"{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}"#,
        ),
        "{}",
        None,
    ))
    .unwrap();
    assert!(envelope.component().capabilities().is_empty());
}
