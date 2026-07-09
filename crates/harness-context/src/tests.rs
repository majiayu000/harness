use crate::providers::{ExecPlanProvider, StaticProvider};
use crate::*;
use harness_core::types::{ProjectId, ThreadId};

fn req() -> ComposeRequest {
    ComposeRequest {
        thread_id: ThreadId::from_str("thread-1"),
        run_id: None,
        project: ProjectId::from_str("project-1"),
        task_profile: TaskProfile::default(),
        budget_hint: 100,
    }
}

fn item(id: &str, class: ItemClass, content_len: usize, priority: Priority) -> ContextItem {
    ContextItem {
        id: ItemId::new(id),
        class,
        content: "x".repeat(content_len),
        est_tokens: 0,
        priority,
        relevance: 1.0,
        degrade: vec![
            Degraded::Summary(format!("summary {id}")),
            Degraded::Pointer(format!("pointer {id}")),
        ],
        dedupe_key: None,
        instruction_bearing: false,
    }
}

#[test]
fn types_bytes_div_four_estimator_documents_v1_behavior() {
    let estimator = BytesDivFourEstimator;
    assert_eq!(estimator.estimate(""), 0);
    assert_eq!(estimator.estimate("abcd"), 1);
    assert_eq!(estimator.estimate("abcde"), 1);
}

#[test]
fn pipeline_is_deterministic_for_identical_inputs() {
    let items = vec![
        item("rule:b", ItemClass::Rule, 20, Priority::P1),
        item("contract:a", ItemClass::Contract, 20, Priority::P0),
    ];
    let composer = ContextComposer::new(ComposeConfig::default())
        .with_provider(Box::new(StaticProvider::new("rules", items)));
    let first = composer.compose(&req()).expect("composition succeeds");
    let second = composer.compose(&req()).expect("composition succeeds");
    assert_eq!(first.rendered, second.rendered);
    assert_eq!(first.manifest, second.manifest);
}

#[test]
fn summarized_degrade_records_nap_and_token_fields_in_manifest() {
    use harness_core::compress::NapStatus;
    let mut compressed_item = item("rule:a", ItemClass::Rule, 400, Priority::P1);
    compressed_item.degrade = vec![Degraded::Summarized {
        text: "x".repeat(40),
        nap: NapStatus::Verified,
    }];
    let config = ComposeConfig {
        reserved_headroom: 0.0,
        ..Default::default()
    };
    let composer = ContextComposer::new(config).with_provider(Box::new(StaticProvider::new(
        "rules",
        vec![compressed_item],
    )));
    let mut request = req();
    request.budget_hint = 30;
    let result = composer.compose(&request).expect("composition succeeds");
    let entry = result
        .manifest
        .items
        .iter()
        .find(|entry| entry.id.as_str() == "rule:a")
        .expect("manifest entry exists");
    assert_eq!(entry.decision, ManifestDecision::Degraded);
    assert_eq!(entry.level.as_deref(), Some("summarized"));
    assert_eq!(entry.nap, Some(NapStatus::Verified));
    assert_eq!(entry.original_tokens, Some(100));
    assert_eq!(entry.compressed_tokens, Some(10));
    assert!(result.rendered.contains(&"x".repeat(40)));
}

#[test]
fn pipeline_enforces_budget_and_degrades_or_excludes() {
    let config = ComposeConfig {
        reserved_headroom: 0.0,
        ..Default::default()
    };
    let composer = ContextComposer::new(config).with_provider(Box::new(StaticProvider::new(
        "rules",
        vec![
            item("rule:a", ItemClass::Rule, 120, Priority::P1),
            item("rule:b", ItemClass::Rule, 120, Priority::P1),
        ],
    )));
    let mut request = req();
    request.budget_hint = 20;
    let result = composer.compose(&request).expect("composition succeeds");
    assert!(result.manifest.budget.used <= result.manifest.budget.effective);
    assert!(result.manifest.items.iter().any(|item| matches!(
        item.decision,
        ManifestDecision::Degraded | ManifestDecision::Excluded
    )));
}

#[test]
fn pipeline_mandatory_overflow_is_loud_error() {
    let config = ComposeConfig {
        reserved_headroom: 0.0,
        ..Default::default()
    };
    let composer = ContextComposer::new(config).with_provider(Box::new(StaticProvider::new(
        "contract",
        vec![
            item("contract:huge", ItemClass::Contract, 100, Priority::P0),
            item("rule:blocked", ItemClass::Rule, 4, Priority::P1),
        ],
    )));
    let mut request = req();
    request.budget_hint = 10;
    let error = composer.compose(&request).expect_err("P0 overflow fails");
    assert!(matches!(error, ComposeError::MandatoryOverflow { .. }));
    assert!(error
        .manifest()
        .items
        .iter()
        .any(|item| item.reason.as_deref() == Some("mandatory_overflow")));
    assert!(error.manifest().items.iter().any(|item| {
        item.id.as_str() == "rule:blocked"
            && item.reason.as_deref() == Some("blocked_by_mandatory_overflow")
    }));
}

#[test]
fn manifest_records_dedupe_provider_errors_and_warnings() {
    struct FailingProvider;
    impl ContextProvider for FailingProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("broken")
        }

        fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
            Err(ProviderError::new("broken", "boom"))
        }
    }

    let mut items = Vec::new();
    for idx in 0..16 {
        let mut proposal = item(&format!("rule:{idx:02}"), ItemClass::Rule, 4, Priority::P2);
        proposal.instruction_bearing = true;
        if idx == 1 {
            proposal.dedupe_key = Some("same".to_string());
        }
        if idx == 2 {
            proposal.dedupe_key = Some("same".to_string());
        }
        items.push(proposal);
    }
    let composer = ContextComposer::new(ComposeConfig::default())
        .with_provider(Box::new(StaticProvider::new("rules", items)))
        .with_provider(Box::new(FailingProvider));
    let result = composer.compose(&req()).expect("composition succeeds");
    assert_eq!(result.manifest.provider_errors.len(), 1);
    assert!(result
        .manifest
        .warnings
        .iter()
        .any(|warning| warning.starts_with("constraint_overload:")));
    assert!(result
        .manifest
        .items
        .iter()
        .any(|item| item.reason.as_deref() == Some("dedupe")));
}

#[test]
fn collect_records_provider_timeout_and_continues() {
    struct SlowProvider;
    impl ContextProvider for SlowProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("slow")
        }

        fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(vec![item("rule:late", ItemClass::Rule, 4, Priority::P1)])
        }
    }

    let config = ComposeConfig {
        provider_timeout_ms: 1,
        ..Default::default()
    };
    let composer = ContextComposer::new(config)
        .with_provider(Box::new(SlowProvider))
        .with_provider(Box::new(StaticProvider::new(
            "rules",
            vec![item("rule:on-time", ItemClass::Rule, 4, Priority::P1)],
        )));

    let result = composer.compose(&req()).expect("composition succeeds");

    assert!(result
        .manifest
        .provider_errors
        .iter()
        .any(|error| error.provider_id.as_str() == "slow" && error.message.contains("timed out")));
    assert!(result
        .manifest
        .items
        .iter()
        .any(|item| item.id.as_str() == "rule:on-time"));
    assert!(!result
        .manifest
        .items
        .iter()
        .any(|item| item.id.as_str() == "rule:late"));
}

#[test]
fn manifest_serialization_omits_item_content() {
    let secret_content = "secret instruction body must not be persisted";
    let composer = ContextComposer::new(ComposeConfig::default()).with_provider(Box::new(
        StaticProvider::new(
            "rules",
            vec![ContextItem {
                content: secret_content.to_string(),
                ..item("rule:secret", ItemClass::Rule, 4, Priority::P1)
            }],
        ),
    ));

    let result = composer.compose(&req()).expect("composition succeeds");
    let manifest_json = serde_json::to_string(&result.manifest).expect("manifest serializes");

    assert!(manifest_json.contains("rule:secret"));
    assert!(!manifest_json.contains(secret_content));
}

#[test]
fn providers_include_active_exec_plans_for_matching_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut plan = harness_exec::plan::ExecPlan::from_spec("# Ship context composer", dir.path())
        .expect("plan");
    plan.activate();
    let provider = ExecPlanProvider::new(vec![plan.clone()]);
    let mut request = req();
    request.project = ProjectId::from_path(dir.path());

    let items = provider.propose(&request).expect("provider succeeds");

    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].id.as_str(),
        format!("contract:exec-plan:{}", plan.id)
    );
    assert_eq!(items[0].class, ItemClass::Contract);
    assert_eq!(items[0].priority, Priority::P0);
    assert!(items[0].content.contains("Ship context composer"));
}
