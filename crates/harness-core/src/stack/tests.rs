use super::*;
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use std::path::{Path, PathBuf};

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn assert_wire_roundtrip<T>(values: &[T], expected: &[&str])
where
    T: Copy + Debug + PartialEq + Serialize + DeserializeOwned,
{
    assert_eq!(values.len(), expected.len());
    for (value, wire) in values.iter().zip(expected) {
        let encoded = serde_json::to_string(value).expect("enum serialization");
        assert_eq!(encoded, format!("\"{wire}\""));
        let decoded: T = serde_json::from_str(&encoded).expect("enum deserialization");
        assert_eq!(*value, decoded);
    }
}

fn repository_source(locator: &str) -> AgentStackSource {
    AgentStackSource::new(AgentStackSourceScope::Repository, locator)
        .expect("valid repository source")
}

fn test_component(
    kind: AgentStackComponentKind,
    source: AgentStackSource,
    observation: AgentStackObservationClass,
    selection: AgentStackSelectionState,
    trust: AgentStackTrustLevel,
) -> Result<AgentStackComponent, AgentStackComponentError> {
    AgentStackComponent::new(
        kind,
        source,
        observation,
        selection,
        trust,
        AgentStackFreshness::Unknown,
    )
}

fn base_component() -> AgentStackComponent {
    test_component(
        AgentStackComponentKind::Skill,
        repository_source("skills/example/SKILL.md"),
        AgentStackObservationClass::RepositoryObserved,
        AgentStackSelectionState::Discovered,
        AgentStackTrustLevel::RepositoryObserved,
    )
    .expect("valid base component")
}

fn base_value() -> Value {
    serde_json::to_value(base_component()).expect("base component JSON")
}

fn validation_error(value: Value) -> AgentStackComponentError {
    match AgentStackComponent::from_json(&value.to_string()) {
        Err(AgentStackComponentParseError::Validation(error)) => error,
        Err(other) => panic!("expected validation error, got {other:?}"),
        Ok(_) => panic!("expected validation error, got success"),
    }
}

fn assert_error_variant(error: AgentStackComponentError, expected: &str) {
    let actual = format!("{error:?}");
    assert!(
        actual.starts_with(expected),
        "expected {expected}, got {actual}"
    );
}

fn assert_syntax_value(value: Value) {
    assert!(matches!(
        AgentStackComponent::from_json(&value.to_string()),
        Err(AgentStackComponentParseError::Syntax(_))
    ));
}

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("valid fixed timestamp")
}

fn host_root(name: &str) -> PathBuf {
    std::env::temp_dir().join("harness-stack-tests").join(name)
}

fn assert_source(source: AgentStackSource, scope: AgentStackSourceScope, locator: &str) {
    assert_eq!(source.scope(), scope);
    assert_eq!(source.locator().as_str(), locator);
    let item = test_component(
        AgentStackComponentKind::Skill,
        source,
        AgentStackObservationClass::RepositoryObserved,
        AgentStackSelectionState::Selected,
        AgentStackTrustLevel::SelfDeclared,
    )
    .unwrap();
    let decoded = AgentStackComponent::from_json(&serde_json::to_string(&item).unwrap()).unwrap();
    assert_eq!(decoded.source().scope(), scope);
}

fn assert_logical(scope: AgentStackSourceScope, namespace: &str, path: &str, locator: &str) {
    assert_source(
        AgentStackSource::logical(scope, namespace, path).unwrap(),
        scope,
        locator,
    );
}

macro_rules! stack_tests {
    ($($item:item)*) => { $(#[test] $item)* };
}

stack_tests! {
fn schema_version_is_required_and_exact() {
    assert!(base_component().validate().is_ok());
    assert!(AgentStackComponent::from_json(&base_value().to_string()).is_ok());

    for replacement in [None, Some(""), Some("agent-stack-component/v9")] {
        let mut value = base_value();
        match replacement {
            Some(version) => value["schema_version"] = json!(version),
            None => {
                value.as_object_mut().unwrap().remove("schema_version");
            }
        }
        assert_error_variant(validation_error(value), "UnsupportedSchemaVersion");
    }

    let mut unknown = base_value();
    unknown["extra"] = json!(true);
    assert_syntax_value(unknown);

    let mut missing = base_value();
    missing.as_object_mut().unwrap().remove("kind");
    assert_syntax_value(missing);

    let mut null_integrity = base_value();
    null_integrity["integrity"] = Value::Null;
    assert_syntax_value(null_integrity);
}

fn unsupported_version_precedes_strict_v01_shape_validation() {
    let mut future = base_value();
    future["schema_version"] = json!("agent-stack-component/v0.2");
    future["future_only_field"] = json!({"shape": "unknown to v0.1"});
    assert_error_variant(validation_error(future), "UnsupportedSchemaVersion");

    assert!(matches!(
        AgentStackComponent::from_json("{not-json"),
        Err(AgentStackComponentParseError::Syntax(_))
    ));

    let mut invalid_locator = base_value();
    invalid_locator["source"]["locator"] = json!("../escape");
    assert_error_variant(validation_error(invalid_locator), "InvalidSourceLocator");
}

fn wire_parser_is_environment_independent() {
    let source = AgentStackSource::new(
        AgentStackSourceScope::UserGlobal,
        "home_harness/skills/example/SKILL.md",
    )
    .unwrap();
    let item = test_component(
        AgentStackComponentKind::Skill,
        source,
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::RuntimeObserved,
    )
    .unwrap();
    let value = serde_json::to_string(&item).unwrap();
    let first = temp_env::with_vars(
        [("HOME", Some("/tmp/first")), ("XDG_CONFIG_HOME", None)],
        || AgentStackComponent::from_json(&value).unwrap(),
    );
    let second = temp_env::with_vars(
        [("HOME", Some("/tmp/second")), ("XDG_CONFIG_HOME", Some("relative"))],
        || AgentStackComponent::from_json(&value).unwrap(),
    );
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

fn all_component_values_round_trip_in_canonical_wire_order() {
    for kind in AgentStackComponentKind::ALL {
        let item = test_component(
            *kind,
            repository_source("components/example"),
            AgentStackObservationClass::RepositoryObserved,
            AgentStackSelectionState::Eligible,
            AgentStackTrustLevel::SelfDeclared,
        )
        .unwrap()
        .with_integrity(Some(Sha256Digest::parse(HASH_A).unwrap()))
        .with_capabilities([
            AgentStackCapability::Shell,
            AgentStackCapability::Destructive,
            AgentStackCapability::FileWrite,
        ])
        .unwrap();
        let encoded = serde_json::to_string(&item).unwrap();
        let decoded = AgentStackComponent::from_json(&encoded).unwrap();
        assert_eq!(decoded.kind(), *kind);
        assert_eq!(
            decoded
                .capabilities()
                .iter()
                .map(AgentStackCapability::as_str)
                .collect::<Vec<_>>(),
            ["destructive", "file_write", "shell"]
        );
    }

    let expected = "{\"schema_version\":\"agent-stack-component/v0.1\",\"component_id\":\"repository:skill:skills/example/SKILL.md\",\"kind\":\"skill\",\"source\":{\"scope\":\"repository\",\"locator\":\"skills/example/SKILL.md\"},\"observation_class\":\"repository_observed\",\"selection_state\":\"discovered\",\"capabilities\":[],\"trust_level\":\"repository_observed\",\"freshness\":\"unknown\"}";
    assert_eq!(serde_json::to_string(&base_component()).unwrap(), expected);

    let duplicate = base_component()
        .with_capabilities([AgentStackCapability::Shell, AgentStackCapability::Shell]);
    assert_error_variant(duplicate.unwrap_err(), "DuplicateCapability");

    let mut duplicate_wire = base_value();
    duplicate_wire["capabilities"] = json!(["shell", "shell"]);
    assert_error_variant(validation_error(duplicate_wire), "DuplicateCapability");
}

#[rustfmt::skip]
fn component_kind_wire_vocabulary_is_closed() { assert_wire_roundtrip(AgentStackComponentKind::ALL, &["instructions", "skill", "mcp_server", "mcp_tool", "hook", "memory", "policy", "workflow", "validation", "agent_runtime"]); for invalid in ["instruction", "Skill", "mcp-server", "unknown"] { assert!(serde_json::from_str::<AgentStackComponentKind>(&format!("\"{invalid}\"")).is_err()); } }

fn explicit_multi_role_bindings_have_distinct_component_ids() {
    let source = repository_source("shared/definition.md");
    let skill = AgentStackComponentId::from_source(AgentStackComponentKind::Skill, &source);
    let policy = AgentStackComponentId::from_source(AgentStackComponentKind::Policy, &source);
    assert_eq!(skill.as_str(), "repository:skill:shared/definition.md");
    assert_eq!(policy.as_str(), "repository:policy:shared/definition.md");
    assert_ne!(skill.as_str(), policy.as_str());
}

#[rustfmt::skip]
fn capability_wire_vocabulary_is_closed() { assert_wire_roundtrip(AgentStackCapability::ALL, &["destructive", "secret_read", "network", "privileged", "production_write", "shell", "file_write"]); for invalid in ["write", "SecretRead", "secret-read", "unknown"] { assert!(serde_json::from_str::<AgentStackCapability>(&format!("\"{invalid}\"")).is_err()); } }

#[rustfmt::skip]
fn remaining_wire_vocabularies_are_closed() { assert_wire_roundtrip(AgentStackSourceScope::ALL, &["repository", "user_global", "admin", "system", "runtime", "runner"]); assert_wire_roundtrip(AgentStackUserGlobalRoot::ALL, &["home_harness", "xdg_config_harness", "platform_config_harness", "configured_user"]); assert_wire_roundtrip(AgentStackSelectionState::ALL, &["discovered", "eligible", "selected", "loaded", "observed"]); assert_wire_roundtrip(AgentStackTrustLevel::ALL, &["self_declared", "repository_observed", "runtime_observed", "runner_observed"]); assert_wire_roundtrip(AgentStackFreshness::ALL, &["unknown", "fresh", "stale", "expired"]); }

fn observation_class_round_trips_without_implied_trust() {
    assert_wire_roundtrip(
        AgentStackObservationClass::ALL,
        &["repository_observed", "runtime_observed", "runner_observed"],
    );
    let item = test_component(
        AgentStackComponentKind::Workflow,
        repository_source("WORKFLOW.md"),
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        AgentStackTrustLevel::SelfDeclared,
    )
    .unwrap();
    assert_eq!(item.trust_level(), AgentStackTrustLevel::SelfDeclared);
}

fn selection_state_requires_supporting_observation() {
    for observation in AgentStackObservationClass::ALL {
        for selection in AgentStackSelectionState::ALL {
            let expected = !matches!(
                (observation, selection),
                (
                    AgentStackObservationClass::RepositoryObserved,
                    AgentStackSelectionState::Loaded | AgentStackSelectionState::Observed
                )
            );
            let result = test_component(
                AgentStackComponentKind::Instructions,
                repository_source("AGENTS.md"),
                *observation,
                *selection,
                AgentStackTrustLevel::SelfDeclared,
            );
            assert_eq!(result.is_ok(), expected, "{observation:?} × {selection:?}");
            if let Err(error) = result {
                assert_error_variant(error, "SelectionNotSupported");
            }
        }
    }
}

fn trust_cannot_exceed_observation_source() {
    for observation in AgentStackObservationClass::ALL {
        for trust in AgentStackTrustLevel::ALL {
            let expected = match observation {
                AgentStackObservationClass::RepositoryObserved => matches!(
                    trust,
                    AgentStackTrustLevel::SelfDeclared | AgentStackTrustLevel::RepositoryObserved
                ),
                AgentStackObservationClass::RuntimeObserved => {
                    !matches!(trust, AgentStackTrustLevel::RunnerObserved)
                }
                AgentStackObservationClass::RunnerObserved => true,
            };
            let result = test_component(
                AgentStackComponentKind::Validation,
                repository_source("checks/specrail.py"),
                *observation,
                AgentStackSelectionState::Selected,
                *trust,
            );
            assert_eq!(result.is_ok(), expected, "{observation:?} × {trust:?}");
            if let Err(error) = result {
                assert_error_variant(error, "TrustExceedsObservation");
            }
        }
    }
}

fn component_identity_is_stable_across_observation_classes() {
    let id = |observation, selection| {
        test_component(
            AgentStackComponentKind::Skill,
            repository_source("skills/stable/SKILL.md"),
            observation,
            selection,
            AgentStackTrustLevel::RepositoryObserved,
        )
        .unwrap()
        .component_id()
        .as_str()
        .to_string()
    };
    let ids = [
        id(
            AgentStackObservationClass::RepositoryObserved,
            AgentStackSelectionState::Selected,
        ),
        id(
            AgentStackObservationClass::RuntimeObserved,
            AgentStackSelectionState::Loaded,
        ),
        id(
            AgentStackObservationClass::RunnerObserved,
            AgentStackSelectionState::Observed,
        ),
    ];
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]));
}

fn missing_freshness_is_explicitly_unknown() {
    let evidence = AgentStackFreshnessEvidence::new(false, None, None, false, false);
    assert_eq!(evidence.classify(), AgentStackFreshness::Unknown);
    assert!(!evidence.authoritatively_invalidated());
    assert!(evidence.observation_time().is_none());
    assert!(evidence.valid_until().is_none());
    assert!(!evidence.current_source_observed());
    assert!(!evidence.cached_prior_observation());
}

fn freshness_evidence_mapping_is_deterministic() {
    let before = timestamp(10);
    let deadline = timestamp(20);
    let cases = [
        (
            AgentStackFreshnessEvidence::new(false, Some(before), Some(deadline), true, true),
            AgentStackFreshness::Fresh,
        ),
        (
            AgentStackFreshnessEvidence::new(false, None, None, false, true),
            AgentStackFreshness::Stale,
        ),
        (
            AgentStackFreshnessEvidence::new(false, Some(before), None, false, false),
            AgentStackFreshness::Unknown,
        ),
        (
            AgentStackFreshnessEvidence::new(false, None, Some(deadline), false, false),
            AgentStackFreshness::Unknown,
        ),
    ];
    for (evidence, expected) in cases {
        assert_eq!(evidence.classify(), expected);
        assert_eq!(evidence.classify(), expected);
    }
}

fn freshness_deadline_is_expired_at_exact_boundary() {
    let boundary = timestamp(100);
    let evidence =
        AgentStackFreshnessEvidence::new(false, Some(boundary), Some(boundary), false, false);
    assert_eq!(evidence.classify(), AgentStackFreshness::Expired);
}

fn explicit_expiry_precedes_current_and_cached_evidence() {
    let evidence = AgentStackFreshnessEvidence::new(true, None, None, true, true);
    assert_eq!(evidence.classify(), AgentStackFreshness::Expired);

    let deadline = AgentStackFreshnessEvidence::new(
        false,
        Some(timestamp(101)),
        Some(timestamp(100)),
        true,
        true,
    );
    assert_eq!(deadline.classify(), AgentStackFreshness::Expired);
}

fn cached_without_current_observation_is_stale() {
    let evidence = AgentStackFreshnessEvidence::new(false, None, None, false, true);
    assert_eq!(evidence.classify(), AgentStackFreshness::Stale);
}

fn source_mapping_contract_examples_are_canonical() {
    let repo_root = host_root("repository");
    assert_source(
        AgentStackSource::repository_from_path(&repo_root, &repo_root.join("AGENTS.md")).unwrap(),
        AgentStackSourceScope::Repository,
        "AGENTS.md",
    );
    let home_root = host_root("user").join(".harness");
    assert_source(
        AgentStackSource::user_global_from_path(
            &home_root.join("skills/a/SKILL.md"),
            Some(&home_root),
            None,
            None,
            &[],
        )
        .unwrap(),
        AgentStackSourceScope::UserGlobal,
        "home_harness/skills/a/SKILL.md",
    );
    #[cfg(unix)]
    let admin = AgentStackSource::admin_from_path(Path::new("/etc/harness/rules/base.md")).unwrap();
    #[cfg(not(unix))]
    let admin = AgentStackSource::new(AgentStackSourceScope::Admin, "rules/base.md").unwrap();
    #[cfg(windows)]
    assert!(AgentStackSource::admin_from_path(Path::new(r"C:\ProgramData\Harness\rules\base.md")).is_err());
    assert_source(admin, AgentStackSourceScope::Admin, "rules/base.md");
    assert_logical(
        AgentStackSourceScope::System,
        "core",
        "golden-principles.md",
        "builtin/core/golden-principles.md",
    );
    assert_logical(
        AgentStackSourceScope::Runtime,
        "workflow_runtime",
        "codex-default",
        "workflow_runtime/codex-default",
    );
    assert_logical(
        AgentStackSourceScope::Runner,
        "probe",
        "getUser",
        "probe/getUser",
    );
}

fn user_global_root_selection_collapses_overlaps_by_precedence() {
    let root = host_root("same-root");
    let source = root.join("skills/a/SKILL.md");
    let (selected, locator) =
        select_user_global_root(&source, Some(&root), Some(&root), Some(&root), &[]).unwrap();
    assert_eq!(selected, AgentStackUserGlobalRoot::HomeHarness);
    assert_eq!(locator.as_str(), "home_harness/skills/a/SKILL.md");

    let parent = host_root("overlap");
    let nested = parent.join("nested");
    let source = nested.join("config.toml");
    let (selected, _) =
        select_user_global_root(&source, Some(&parent), Some(&nested), None, &[]).unwrap();
    assert_eq!(selected, AgentStackUserGlobalRoot::HomeHarness);

    let outside_prefix = parent.with_file_name("overlap-suffix").join("item");
    let error =
        select_user_global_root(&outside_prefix, Some(&parent), None, None, &[]).unwrap_err();
    assert_error_variant(error, "UntypedDiscoverySource");
    let error =
        select_user_global_root(&root.join("missing"), Some(&root), None, None, &[]).unwrap_err();
    assert_error_variant(error, "InvalidSourceLocator");
}

fn multiple_configured_user_roots_fail_as_ambiguous() {
    let root = host_root("configured");
    let nested = root.join("nested");
    let configured = [("primary", root.as_path()), ("nested", nested.as_path())];
    let error = select_user_global_root(&root.join("nested/item"), None, None, None, &configured)
        .unwrap_err();
    assert_error_variant(error, "AmbiguousConfiguredUserRoot");
}

fn configured_user_key_uses_strict_snake_case() {
    let root = host_root("configured-valid");
    let source = AgentStackSource::user_global_from_path(
        &root.join("skills/example"),
        None,
        None,
        None,
        &[("team_one2", root.as_path())],
    )
    .unwrap();
    assert_eq!(
        source.locator().as_str(),
        "configured_user/team_one2/skills/example"
    );
}

fn configured_user_key_rejects_display_uuid_and_reserved_segments() {
    let root = host_root("configured-invalid");
    for key in [
        "Team_one",
        "team-one",
        "display name",
        "unknown",
        "550e8400-e29b-41d4-a716-446655440000",
    ] {
        let error = AgentStackSource::user_global_from_path(
            &root.join("item"),
            None,
            None,
            None,
            &[(key, root.as_path())],
        )
        .unwrap_err();
        assert_error_variant(error, "InvalidSourceLocator");
    }
}

fn xdg_root_falls_back_to_absolute_home_when_xdg_is_missing_or_relative() {
    let home = host_root("home");
    let expected = home.join(".config").join("harness");
    assert_eq!(
        resolve_xdg_config_harness_root(None, Some(&home)).unwrap(),
        expected
    );
    assert_eq!(
        resolve_xdg_config_harness_root(Some(Path::new("relative")), Some(&home)).unwrap(),
        expected
    );

    let absolute_xdg = host_root("xdg");
    assert_eq!(
        resolve_xdg_config_harness_root(Some(&absolute_xdg), Some(&home)).unwrap(),
        absolute_xdg.join("harness")
    );
}

fn xdg_root_fails_when_xdg_and_home_are_unusable() {
    for (xdg, home) in [
        (None, None),
        (Some(Path::new("relative-xdg")), None),
        (
            Some(Path::new("relative-xdg")),
            Some(Path::new("relative-home")),
        ),
    ] {
        let error = resolve_xdg_config_harness_root(xdg, home).unwrap_err();
        assert_error_variant(error, "XdgConfigRootUnavailable");
    }
}

fn path_locator_rejects_non_utf8_without_lossy_conversion() {
    #[cfg(unix)]
    let (root, source) = {
        use std::os::unix::ffi::OsStringExt;
        let root = PathBuf::from("/repo");
        let source = root.join(std::ffi::OsString::from_vec(b"bad-\xff".to_vec()));
        (root, source)
    };
    #[cfg(windows)]
    let (root, source) = {
        use std::os::windows::ffi::OsStringExt;
        let root = PathBuf::from(r"C:\repo");
        let source = root.join(std::ffi::OsString::from_wide(&[0xd800]));
        (root, source)
    };
    assert_error_variant(
        AgentStackSource::repository_from_path(&root, &source).unwrap_err(),
        "NonUtf8SourceLocator",
    );
}

fn portable_segment_encoder_uses_forward_slashes() {
    let root = host_root("portable");
    let source = AgentStackSource::repository_from_path(
        &root,
        &root.join("skills").join("nested").join("SKILL.md"),
    )
    .unwrap();
    assert_eq!(source.locator().as_str(), "skills/nested/SKILL.md");
    assert!(!source.locator().as_str().contains('\\'));
}

fn path_adapter_canonicalizes_curdir_and_rejects_parentdir() {
    let root = host_root("components");
    let canonical =
        AgentStackSource::repository_from_path(&root, &root.join("a").join(".").join("b")).unwrap();
    assert_eq!(canonical.locator().as_str(), "a/b");

    let error = AgentStackSource::repository_from_path(&root, &root.join("a").join("..").join("b"))
        .unwrap_err();
    assert_error_variant(error, "InvalidSourceLocator");

    let outside = root.parent().unwrap().join("outside");
    let error = AgentStackSource::repository_from_path(&root, &outside).unwrap_err();
    assert_error_variant(error, "SourceOutsideRoot");
}

fn windows_drive_letter_casing_is_canonical_equivalent() {
    assert!(super::root_keys_match_for_test(
        Some(b'c'),
        &["Root", "Project"],
        Some(b'C'),
        &["Root", "Project"],
    ));
}

fn windows_directory_segment_casing_remains_distinct() {
    assert!(!super::root_keys_match_for_test(
        Some(b'C'),
        &["Root", "Project"],
        Some(b'c'),
        &["root", "Project"],
    ));
}

fn logical_path_grammar_covers_system_runtime_and_runner() {
    assert_logical(
        AgentStackSourceScope::System,
        "core_rules",
        "exec-plan/golden-principles.md",
        "builtin/core_rules/exec-plan/golden-principles.md",
    );
    assert_logical(
        AgentStackSourceScope::Runtime,
        "workflow_runtime",
        "DATA_EXPORT_v2",
        "workflow_runtime/DATA_EXPORT_v2",
    );
    assert_logical(
        AgentStackSourceScope::Runner,
        "mcp_probe",
        "getUser/v2.1",
        "mcp_probe/getUser/v2.1",
    );
    for (namespace, path) in [
        ("Bad-Namespace", "valid"),
        ("0123456789abcdef0123456789abcdef", "tool"),
        ("valid", ""),
        ("valid", "-leading"),
        ("valid", ".."),
        ("valid", "display name"),
        ("valid", "550e8400-e29b-41d4-a716-446655440000"),
    ] {
        let error =
            AgentStackSource::logical(AgentStackSourceScope::Runtime, namespace, path).unwrap_err();
        assert_error_variant(error, "InvalidSourceLocator");
    }
}

fn logical_path_preserves_case_distinct_tool_names() {
    let upper = AgentStackSource::logical(AgentStackSourceScope::Runner, "mcp", "getUser").unwrap();
    let lower = AgentStackSource::logical(AgentStackSourceScope::Runner, "mcp", "getuser").unwrap();
    assert_eq!(upper.locator().as_str(), "mcp/getUser");
    assert_eq!(lower.locator().as_str(), "mcp/getuser");
    assert_ne!(upper.locator().as_str(), lower.locator().as_str());
}

fn untyped_custom_discovery_source_fails_closed() {
    let source = host_root("untyped").join("custom/item");
    let error =
        AgentStackSource::user_global_from_path(&source, None, None, None, &[]).unwrap_err();
    assert_error_variant(error, "UntypedDiscoverySource");
}

fn source_locator_rejects_reserved_sentinels() {
    for (scope, locator) in [
        (AgentStackSourceScope::Repository, "unknown"),
        (AgentStackSourceScope::Admin, "rules/null"),
        (AgentStackSourceScope::UserGlobal, "home_harness/missing"),
        (AgentStackSourceScope::UserGlobal, "custom/item"),
        (AgentStackSourceScope::Runtime, "0123456789abcdef0123456789abcdef/tool"),
        (AgentStackSourceScope::Repository, "/absolute"),
        (AgentStackSourceScope::Repository, "C:/drive"),
        (AgentStackSourceScope::Repository, r"a\b"),
        (AgentStackSourceScope::Repository, "a//b"),
        (AgentStackSourceScope::Repository, "a/./b"),
        (AgentStackSourceScope::Repository, "a/../b"),
    ] {
        let error = AgentStackSource::new(scope, locator).unwrap_err();
        assert_error_variant(error, "InvalidSourceLocator");
    }
}

fn runtime_locator_rejects_reserved_segments() {
    for path in [
        "unknown_component/item",
        "item/null",
        "missing",
        "550e8400-e29b-41d4-a716-446655440000",
        "0123456789abcdef0123456789abcdef",
        "display name",
    ] {
        let error =
            AgentStackSource::logical(AgentStackSourceScope::Runtime, "runtime", path).unwrap_err();
        assert_error_variant(error, "InvalidSourceLocator");
    }
}

fn portable_path_locator_rejects_nul() {
    let error =
        AgentStackSource::new(AgentStackSourceScope::Repository, "skills/bad\0name").unwrap_err();
    assert_error_variant(error, "InvalidSourceLocator");
}

fn source_locator_validation_precedes_component_id_derivation() {
    let mut value = base_value();
    value["component_id"] = json!("repository:skill:wrong");
    value["source"]["locator"] = json!("../escape");
    assert_error_variant(validation_error(value), "InvalidSourceLocator");

    let mut id_only = base_value();
    id_only["component_id"] = json!("repository:skill:wrong");
    assert_error_variant(validation_error(id_only), "NonCanonicalComponentId");
}

fn sha256_digest_rejects_blank_malformed_and_mixed_case_values() {
    for invalid in [
        "",
        "abc",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg",
    ] {
        assert_error_variant(
            Sha256Digest::parse(invalid).unwrap_err(),
            "InvalidSha256Digest",
        );
    }

    let mut value = base_value();
    value["integrity"] = json!("bad");
    assert_error_variant(validation_error(value), "InvalidSha256Digest");
}

fn sha256_digest_hashes_exact_source_bytes() {
    let digest = Sha256Digest::from_bytes(b"abc");
    assert_eq!(
        digest.as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

fn sha256_digest_distinguishes_lf_crlf_bom_and_unicode_bytes() {
    let inputs: &[&[u8]] = &[
        b"x\n",
        b"x\r\n",
        b"\xef\xbb\xbfx\n",
        "é\n".as_bytes(),
        &[0x00, 0xe9, 0x00, 0x0a],
    ];
    let digests = inputs
        .iter()
        .map(|bytes| Sha256Digest::from_bytes(bytes).as_str().to_string())
        .collect::<Vec<_>>();
    for left in 0..digests.len() {
        for right in (left + 1)..digests.len() {
            assert_ne!(digests[left], digests[right]);
        }
    }
}

fn empty_content_digest_is_distinct_from_missing_integrity() {
    let empty = Sha256Digest::from_bytes(b"");
    assert_eq!(
        empty.as_str(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let absent = base_component();
    assert!(absent.integrity().is_none());
}

fn missing_optional_facts_are_not_fabricated() {
    let component = base_component().with_integrity(None);
    assert!(component.integrity().is_none());
    let value = serde_json::to_value(component).unwrap();
    assert!(value.get("integrity").is_none());
    let encoded = value.to_string();
    assert!(!encoded.contains("unknown-component"));
    assert!(!encoded.contains("0000000000000000000000000000000000000000000000000000000000000000"));
}
}
