use super::*;
use crate::workflow_runtime_worker::prompt_packet::{
    build_runtime_job_prompt, build_runtime_prompt_packet, prompt_packet_digest,
    workflow_prompt_artifact,
};
use crate::workflow_runtime_worker::runtime_profile::{
    resolve_runtime_settings, ResolvedApprovalPolicy, RuntimeSettingsResolutionError,
};
use harness_core::config::agents::AgentsConfig;
use harness_core::config::concurrency::ConcurrencyConfig;
use harness_core::config::workflow::WorkflowSourceRole::{CentralBase, RepositoryOverride};
use harness_core::types::ExecutionPhase;
use harness_workflow::runtime::{
    RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord, RuntimeKind, RuntimeProfile,
};
use std::path::{Path, PathBuf};

fn runtime_job(activity: &str) -> RuntimeJob {
    RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": activity }),
    )
}

fn codex_profile() -> RuntimeProfile {
    let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    profile.timeout_secs = Some(3600);
    profile
}

fn resolved(profile: &RuntimeProfile, phase: Option<ExecutionPhase>) -> ResolvedRuntimeSettings {
    resolve_runtime_settings(
        profile,
        profile.kind,
        phase,
        &AgentsConfig::default(),
        &ConcurrencyConfig::default(),
    )
    .unwrap_or_else(|error| panic!("test runtime settings should resolve: {error}"))
}

fn build_packet(
    job: &RuntimeJob,
    profile: &RuntimeProfile,
    workflow_document: &WorkflowDocument,
    repo_memory: &[RetrievedRepoMemoryRecord],
    prompt_task_text: Option<&str>,
) -> Value {
    build_runtime_prompt_packet(
        job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        profile,
        &resolved(profile, Some(ExecutionPhase::Execution)),
        workflow_document,
        repo_memory,
        prompt_task_text,
    )
    .unwrap_or_else(|error| panic!("prompt packet should build: {error}"))
}

fn source_observation(
    role: WorkflowSourceRole,
    path: &str,
    content: &[u8],
) -> WorkflowSourceObservation {
    WorkflowSourceObservation {
        role,
        path: PathBuf::from(path),
        content_sha256: Sha256Digest::from_bytes(content).as_str().to_owned(),
    }
}

fn memory_record(lesson: &str, evidence_ref: Option<&str>) -> RetrievedRepoMemoryRecord {
    let mut record = RepoMemoryRecord::new(
        "owner/repo",
        "implement_issue",
        RepoMemoryOutcome::Failed,
        RepoMemoryKind::FailureLesson,
        json!({ "lesson": lesson }),
    );
    if let Some(evidence_ref) = evidence_ref {
        record = record.with_evidence_ref(evidence_ref);
    }
    RetrievedRepoMemoryRecord {
        record,
        estimated_tokens: 64,
    }
}

fn entries(packet: &Value) -> &Vec<Value> {
    packet["context_provenance"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("provenance entries should be an array"))
}

fn component_id(entry: &Value) -> &str {
    entry["component"]["component_id"]
        .as_str()
        .unwrap_or_else(|| panic!("component_id should be a string"))
}

#[test]
fn v2_packet_and_artifact_share_schema_and_v1_remains_historical() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    assert_eq!(packet["schema"], RUNTIME_PROMPT_PACKET_SCHEMA);
    assert_eq!(packet["schema"], "harness.runtime.prompt_packet.v2");
    assert_eq!(
        packet["context_provenance"]["schema"],
        CONTEXT_PROVENANCE_SCHEMA
    );
    let artifact = workflow_prompt_artifact(&prompt_packet_digest(&packet));
    assert_eq!(artifact.artifact["schema"], RUNTIME_PROMPT_PACKET_SCHEMA);
    validate_prompt_packet_provenance(&packet)
        .unwrap_or_else(|error| panic!("a freshly built v2 packet should be valid: {error}"));

    // Historical v1 packets stay valid lower-evidence records.
    let historical = json!({ "schema": "harness.runtime.prompt_packet.v1" });
    validate_prompt_packet_provenance(&historical)
        .unwrap_or_else(|error| panic!("historical v1 packets remain acceptable: {error}"));

    // A v2 packet with missing, blank, or unsupported provenance is invalid.
    let missing = json!({ "schema": RUNTIME_PROMPT_PACKET_SCHEMA });
    assert!(validate_prompt_packet_provenance(&missing).is_err());
    let blank = json!({
        "schema": RUNTIME_PROMPT_PACKET_SCHEMA,
        "context_provenance": { "schema": CONTEXT_PROVENANCE_SCHEMA, "entries": [] },
    });
    assert!(validate_prompt_packet_provenance(&blank).is_err());
    let unsupported = json!({
        "schema": RUNTIME_PROMPT_PACKET_SCHEMA,
        "context_provenance": { "schema": "harness.runtime.context_provenance.v9", "entries": [{}] },
    });
    assert!(validate_prompt_packet_provenance(&unsupported).is_err());
    let unknown = json!({ "schema": "harness.runtime.prompt_packet.v9" });
    assert!(validate_prompt_packet_provenance(&unknown).is_err());
}

#[test]
fn provenance_contains_only_runtime_selected_sources() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    // Repository discovery alone must never produce a selected/loaded entry.
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    let entries = entries(&packet);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        component_id(&entries[0]),
        "runtime:agent_runtime:runtime_profile/codex-default"
    );
    assert_eq!(
        component_id(&entries[1]),
        "runtime:workflow:workflow_document/defaults"
    );
    for entry in entries {
        assert_eq!(entry["component"]["observation_class"], "runtime_observed");
        assert_eq!(entry["component"]["selection_state"], "loaded");
    }
}

#[test]
fn all_provenance_entries_validate_against_stack_component_contract() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let workflow_document = WorkflowDocument {
        prompt_template: "Follow the workflow.".to_string(),
        source_path: Some("/central/WORKFLOW.md + /repo/WORKFLOW.md".to_string()),
        sources: vec![
            source_observation(CentralBase, "/central/WORKFLOW.md", b"central"),
            source_observation(RepositoryOverride, "/repo/WORKFLOW.md", b"repository"),
        ],
        ..Default::default()
    };
    let memory = vec![memory_record("lesson", Some("workflow:run-1:event:1"))];
    let packet = build_packet(&job, &profile, &workflow_document, &memory, None);

    let entries = entries(&packet);
    assert_eq!(entries.len(), 5);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry["order"], index as u64);
        let component = AgentStackComponent::from_json(&entry["component"].to_string())
            .unwrap_or_else(|error| {
                panic!("component must round-trip the ASC-001 contract: {error}")
            });
        component
            .validate()
            .unwrap_or_else(|error| panic!("component must satisfy ASC-001 validation: {error}"));
        assert!(
            component.integrity().is_some(),
            "every provenance entry records a digest"
        );
    }
}

#[test]
fn claude_phase_defaults_and_explicit_overrides_match_agent_launch_provenance() {
    let agents = AgentsConfig::default();
    let concurrency = ConcurrencyConfig::default();
    let mut profile = RuntimeProfile::new("claude-default", RuntimeKind::ClaudeCode);
    profile.timeout_secs = Some(3600);

    // Omitted model/effort resolve from the same phase config launch uses.
    let planning = resolve_runtime_settings(
        &profile,
        RuntimeKind::ClaudeCode,
        Some(ExecutionPhase::Planning),
        &agents,
        &concurrency,
    )
    .unwrap_or_else(|error| panic!("claude phase defaults should resolve: {error}"));
    let budget = agents
        .claude
        .reasoning_budget
        .as_ref()
        .unwrap_or_else(|| panic!("default claude config has a reasoning budget"));
    assert_eq!(
        planning.model,
        budget.model_for_phase(ExecutionPhase::Planning)
    );
    assert_eq!(
        planning.reasoning_effort.as_deref(),
        Some(ExecutionPhase::Planning.effort_level())
    );

    // Explicit profile values take precedence over phase fallbacks.
    profile.model = Some("claude-custom".to_string());
    profile.reasoning_effort = Some("low".to_string());
    let explicit = resolve_runtime_settings(
        &profile,
        RuntimeKind::ClaudeCode,
        Some(ExecutionPhase::Planning),
        &agents,
        &concurrency,
    )
    .unwrap_or_else(|error| panic!("explicit claude overrides should resolve: {error}"));
    assert_eq!(explicit.model, "claude-custom");
    assert_eq!(explicit.reasoning_effort.as_deref(), Some("low"));

    // Without a phase the shared fallback is the configured default model.
    profile.model = None;
    profile.reasoning_effort = None;
    let phaseless = resolve_runtime_settings(
        &profile,
        RuntimeKind::ClaudeCode,
        None,
        &agents,
        &concurrency,
    )
    .unwrap_or_else(|error| panic!("phaseless claude profile should resolve: {error}"));
    assert_eq!(phaseless.model, agents.claude.default_model);
    assert_eq!(phaseless.reasoning_effort, None);
}

#[test]
fn provenance_and_agent_launch_share_resolved_runtime_settings_and_reject_zero_timeout() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let resolved_settings = resolved(&profile, Some(ExecutionPhase::Execution));
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    // The packet embeds exactly the resolved value that agent launch uses.
    assert_eq!(
        packet["resolved_runtime_settings"],
        serde_json::to_value(&resolved_settings)
            .unwrap_or_else(|error| panic!("resolved settings serialize: {error}"))
    );
    assert_eq!(packet["resolved_runtime_settings"]["timeout_secs"], 3600);
    assert_eq!(
        packet["resolved_runtime_settings"]["max_turns"],
        Value::Null
    );

    // The runtime entry digest covers that same canonical value.
    let digest = Sha256Digest::from_bytes(
        &serde_json::to_vec(&resolved_settings)
            .unwrap_or_else(|error| panic!("resolved settings serialize: {error}")),
    );
    assert_eq!(
        entries(&packet)[0]["component"]["integrity"],
        digest.as_str()
    );
    let mut changed = resolved_settings.clone();
    changed.timeout_secs = 1800;
    let changed_digest = Sha256Digest::from_bytes(
        &serde_json::to_vec(&changed).unwrap_or_else(|error| panic!("serialize: {error}")),
    );
    assert_ne!(digest.as_str(), changed_digest.as_str());

    // timeout_secs = 0 is rejected with a typed error before the packet.
    let mut zero = codex_profile();
    zero.timeout_secs = Some(0);
    let error = resolve_runtime_settings(
        &zero,
        RuntimeKind::CodexJsonrpc,
        None,
        &AgentsConfig::default(),
        &ConcurrencyConfig::default(),
    )
    .expect_err("zero timeout must be rejected");
    assert_eq!(
        error.downcast_ref::<RuntimeSettingsResolutionError>(),
        Some(&RuntimeSettingsResolutionError::ZeroTimeout {
            profile: "codex-default".to_string()
        })
    );
}

#[test]
fn central_repository_merged_and_default_workflows_have_truthful_provenance() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let central = source_observation(CentralBase, "/etc/harness/config/WORKFLOW.md", b"central");
    let repository = source_observation(RepositoryOverride, "/repo/WORKFLOW.md", b"repository");
    let central_path_digest =
        Sha256Digest::from_bytes("/etc/harness/config/WORKFLOW.md".as_bytes());

    // Central-only.
    let central_only = WorkflowDocument {
        sources: vec![central.clone()],
        ..Default::default()
    };
    let packet = build_packet(&job, &profile, &central_only, &[], None);
    let central_entries = entries(&packet);
    assert_eq!(central_entries.len(), 3);
    assert_eq!(
        component_id(&central_entries[1]),
        format!(
            "runtime:workflow:workflow_source/central/{}",
            central_path_digest.as_str()
        )
    );
    assert_eq!(central_entries[1]["reason"], "workflow_base_selected");
    assert_eq!(
        central_entries[1]["component"]["integrity"],
        central.content_sha256
    );
    assert_eq!(
        component_id(&central_entries[2]),
        "runtime:workflow:workflow_document/effective"
    );
    assert_eq!(central_entries[2]["reason"], "workflow_document_effective");
    // Unsafe absolute paths are redacted, not misclassified as defaults.
    let serialized = packet["context_provenance"].to_string();
    assert!(!serialized.contains("/etc/harness"));
    assert!(!serialized.contains("workflow_document/defaults"));

    // Repository-only.
    let repo_only = WorkflowDocument {
        sources: vec![repository.clone()],
        ..Default::default()
    };
    let packet = build_packet(&job, &profile, &repo_only, &[], None);
    let repo_entries = entries(&packet);
    assert_eq!(repo_entries.len(), 3);
    assert_eq!(
        component_id(&repo_entries[1]),
        "repository:workflow:WORKFLOW.md"
    );
    assert_eq!(repo_entries[1]["reason"], "workflow_repository_selected");
    assert_eq!(
        repo_entries[1]["component"]["integrity"],
        repository.content_sha256
    );

    // Merged central + repository keeps ordered truthful source identities.
    let merged = WorkflowDocument {
        prompt_template: "merged".to_string(),
        sources: vec![central.clone(), repository.clone()],
        ..Default::default()
    };
    let packet = build_packet(&job, &profile, &merged, &[], None);
    let merged_entries = entries(&packet);
    assert_eq!(merged_entries.len(), 4);
    assert!(
        component_id(&merged_entries[1]).starts_with("runtime:workflow:workflow_source/central/")
    );
    assert_eq!(
        component_id(&merged_entries[2]),
        "repository:workflow:WORKFLOW.md"
    );
    assert_eq!(
        component_id(&merged_entries[3]),
        "runtime:workflow:workflow_document/effective"
    );

    // No configured source: an explicit defaults entry, no invented file.
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);
    let default_entries = entries(&packet);
    assert_eq!(default_entries.len(), 2);
    assert_eq!(
        component_id(&default_entries[1]),
        "runtime:workflow:workflow_document/defaults"
    );
    assert_eq!(default_entries[1]["reason"], "workflow_defaults_selected");
    assert!(!packet["context_provenance"]
        .to_string()
        .contains("repository:workflow:WORKFLOW.md"));
}

#[test]
fn defaults_entry_digest_is_deterministic_across_builds() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let first = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);
    let second = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    let expected = Sha256Digest::from_bytes(
        &serde_json::to_vec(&CanonicalWorkflowDocumentDigestInput {
            config: &WorkflowConfig::default(),
            prompt_template: "",
        })
        .unwrap_or_else(|error| panic!("default document digest input serializes: {error}")),
    );
    assert_eq!(
        entries(&first)[1]["component"]["integrity"],
        expected.as_str()
    );
    assert_eq!(
        entries(&first)[1]["component"]["integrity"],
        entries(&second)[1]["component"]["integrity"]
    );
}

#[test]
fn selected_memory_order_and_safe_metadata_are_preserved() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let first = memory_record("first lesson", Some("workflow:run-1:event:1"));
    let second = memory_record("second lesson", None);
    let memory = vec![first.clone(), second.clone()];
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &memory, None);

    let entries = entries(&packet);
    // Runtime entry, defaults entry, then memory records in selection order.
    assert_eq!(entries.len(), 4);
    for (entry, record) in entries[2..].iter().zip(&memory) {
        assert_eq!(
            component_id(entry),
            format!("runtime:memory:repo_memory/record-{}", record.record.id)
        );
        assert_eq!(entry["reason"], "repo_memory_selected");
        assert_eq!(entry["component"]["kind"], "memory");
        assert_eq!(entry["component"]["observation_class"], "runtime_observed");
        assert_eq!(entry["component"]["trust_level"], "self_declared");
        assert_eq!(
            entry["memory_metadata"]["record_id"],
            record.record.id.to_string()
        );
        assert_eq!(entry["memory_metadata"]["estimated_tokens"], 64);
        let expected_digest = Sha256Digest::from_bytes(
            &serde_json::to_vec(&repo_memory_record_value(record))
                .unwrap_or_else(|error| panic!("record serializes: {error}")),
        );
        assert_eq!(entry["component"]["integrity"], expected_digest.as_str());
    }
    assert_eq!(
        entries[2]["memory_metadata"]["evidence_ref"],
        "workflow:run-1:event:1"
    );
    assert!(entries[3]["memory_metadata"].get("evidence_ref").is_none());
    // Different durable identities stay distinguishable.
    assert_ne!(component_id(&entries[2]), component_id(&entries[3]));
}

#[test]
fn missing_memory_records_are_not_fabricated() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    // Failed retrieval reaches the builder as an empty record list.
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    assert!(packet.get("repo_memory").is_none());
    assert!(entries(&packet)
        .iter()
        .all(|entry| entry["component"]["kind"] != "memory"));
}

#[test]
fn prompt_task_text_is_digest_bound_without_becoming_context() {
    let mut job = runtime_job("implement_prompt");
    job.input = json!({
        "activity": "implement_prompt",
        "command": { "prompt_ref": "prompt-1" },
    });
    let profile = codex_profile();
    let task_text = "Rename the internal helper and update its tests.";
    let packet = build_packet(
        &job,
        &profile,
        &WorkflowDocument::default(),
        &[],
        Some(task_text),
    );

    assert_eq!(packet["prompt_task_request"]["prompt_ref"], "prompt-1");
    assert_eq!(
        packet["prompt_task_request"]["task_text_sha256"],
        Sha256Digest::from_bytes(task_text.as_bytes()).as_str()
    );
    // Raw task text never becomes packet content or a context entry.
    assert!(!packet.to_string().contains(task_text));
    assert_eq!(entries(&packet).len(), 2);

    // Same reference, changed task text: the packet digest changes.
    let changed = build_packet(
        &job,
        &profile,
        &WorkflowDocument::default(),
        &[],
        Some("A completely different task."),
    );
    assert_ne!(
        prompt_packet_digest(&packet),
        prompt_packet_digest(&changed)
    );

    // Task text without a durable reference is a visible error.
    let mut missing_ref = runtime_job("implement_prompt");
    missing_ref.input = json!({ "activity": "implement_prompt" });
    let error = build_runtime_prompt_packet(
        &missing_ref,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &profile,
        &resolved(&profile, None),
        &WorkflowDocument::default(),
        &[],
        Some(task_text),
    )
    .expect_err("task text without a prompt_ref must fail packet construction");
    assert!(error.to_string().contains("prompt_ref"));
}

#[test]
fn manifest_declares_unobserved_external_context() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);

    assert_eq!(
        packet["context_provenance"]["not_observed_by_harness"],
        json!([
            "agent_cli_context_not_observed",
            "mcp_host_context_not_observed",
            "user_global_context_not_observed",
            "model_provider_context_not_observed",
        ])
    );
}

#[test]
fn codex_omitted_approval_policy_is_recorded_unobserved() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let resolved_settings = resolved(&profile, Some(ExecutionPhase::Execution));

    // Omitted policy: resolved outside Harness, never fabricated here.
    assert_eq!(
        resolved_settings.approval_policy,
        ResolvedApprovalPolicy::UnobservedAgentDefault
    );
    assert_eq!(resolved_settings.approval_policy.explicit_value(), None);
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &[], None);
    assert_eq!(
        packet["resolved_runtime_settings"]["approval_policy"]["resolution"],
        "unobserved_agent_default"
    );
    assert!(packet["context_provenance"]["not_observed_by_harness"]
        .as_array()
        .is_some_and(|markers| markers.contains(&json!("agent_cli_context_not_observed"))));

    // An explicit profile policy is recorded and passed to launch verbatim.
    let mut explicit = codex_profile();
    explicit.approval_policy = Some("on-request".to_string());
    let resolved_explicit = resolved(&explicit, Some(ExecutionPhase::Execution));
    assert_eq!(
        resolved_explicit.approval_policy,
        ResolvedApprovalPolicy::Explicit {
            value: "on-request".to_string()
        }
    );
    assert_eq!(
        resolved_explicit.approval_policy.explicit_value(),
        Some("on-request")
    );
}

#[test]
fn resolved_stall_timeout_matches_lifecycle_normalization() {
    let concurrency = ConcurrencyConfig {
        stall_timeout_secs: 5,
        ..ConcurrencyConfig::default()
    };
    let profile = codex_profile();
    let resolved_settings = resolve_runtime_settings(
        &profile,
        RuntimeKind::CodexJsonrpc,
        None,
        &AgentsConfig::default(),
        &concurrency,
    )
    .unwrap_or_else(|error| panic!("stall timeout should resolve: {error}"));

    let expected = harness_core::config::stall_timeout::normalize_stall_timeout_secs(5, Some(3600));
    assert_eq!(
        resolved_settings.stall_timeout_secs,
        expected.effective_secs
    );
    // The lifecycle re-normalization is idempotent for the resolved value.
    let renormalized = harness_core::config::stall_timeout::normalize_stall_timeout_secs(
        resolved_settings.stall_timeout_secs,
        Some(resolved_settings.timeout_secs),
    );
    assert_eq!(
        renormalized.effective_secs,
        resolved_settings.stall_timeout_secs
    );
    assert!(!renormalized.was_adjusted());
}

#[test]
fn provenance_does_not_duplicate_memory_payload_or_secret_values() {
    let mut job = runtime_job("implement_issue");
    job.input = json!({
        "activity": "implement_issue",
        "command": { "env": { "GITHUB_TOKEN": "ghp-secret-value" } },
    });
    let profile = codex_profile();
    let memory = vec![memory_record(
        "SECRET-PAYLOAD-MARKER API_KEY=sk-live-1234567890",
        Some("workflow:run-1:event:1"),
    )];
    let packet = build_packet(&job, &profile, &WorkflowDocument::default(), &memory, None);

    let provenance = packet["context_provenance"].to_string();
    assert!(!provenance.contains("SECRET-PAYLOAD-MARKER"));
    assert!(!provenance.contains("sk-live-1234567890"));
    assert!(!provenance.contains("ghp-secret-value"));
    let resolved_section = packet["resolved_runtime_settings"].to_string();
    assert!(!resolved_section.contains("ghp-secret-value"));
    // The payload itself remains only in the existing packet memory section.
    assert!(packet["repo_memory"]
        .to_string()
        .contains("SECRET-PAYLOAD-MARKER"));
}

#[test]
fn provenance_and_packet_digests_are_repeatable_and_order_sensitive() {
    let job = runtime_job("implement_issue");
    let profile = codex_profile();
    let first = memory_record("first lesson", None);
    let second = memory_record("second lesson", None);
    let workflow_document = WorkflowDocument {
        sources: vec![source_observation(
            RepositoryOverride,
            "/repo/WORKFLOW.md",
            b"repository",
        )],
        ..Default::default()
    };
    let memory = vec![first.clone(), second.clone()];

    // Rebuilding the same inputs produces identical provenance and digest.
    let packet_a = build_packet(&job, &profile, &workflow_document, &memory, None);
    let packet_b = build_packet(&job, &profile, &workflow_document, &memory, None);
    assert_eq!(
        packet_a["context_provenance"],
        packet_b["context_provenance"]
    );
    assert_eq!(
        prompt_packet_digest(&packet_a),
        prompt_packet_digest(&packet_b)
    );

    // Reordering the same selected sources changes the digest.
    let reordered = vec![second, first];
    let packet_c = build_packet(&job, &profile, &workflow_document, &reordered, None);
    assert_ne!(
        packet_a["context_provenance"],
        packet_c["context_provenance"]
    );
    assert_ne!(
        prompt_packet_digest(&packet_a),
        prompt_packet_digest(&packet_c)
    );

    // Changing a recorded source changes its digest and the packet digest.
    let changed_document = WorkflowDocument {
        sources: vec![source_observation(
            RepositoryOverride,
            "/repo/WORKFLOW.md",
            b"repository-changed",
        )],
        ..Default::default()
    };
    let packet_d = build_packet(&job, &profile, &changed_document, &memory, None);
    assert_ne!(
        entries(&packet_a)[1]["component"]["integrity"],
        entries(&packet_d)[1]["component"]["integrity"]
    );
    assert_ne!(
        prompt_packet_digest(&packet_a),
        prompt_packet_digest(&packet_d)
    );
}

#[test]
fn invalid_required_provenance_aborts_packet_construction() {
    let job = runtime_job("implement_issue");
    // A profile name that cannot form a valid ASC-001 locator must abort
    // packet construction with an error instead of substituting an empty
    // manifest; the executor therefore never hashes, records, or executes
    // the incomplete packet.
    let mut profile = RuntimeProfile::new("bad profile name", RuntimeKind::CodexJsonrpc);
    profile.timeout_secs = Some(3600);
    let resolved_settings = resolve_runtime_settings(
        &profile,
        RuntimeKind::CodexJsonrpc,
        None,
        &AgentsConfig::default(),
        &ConcurrencyConfig::default(),
    )
    .unwrap_or_else(|error| panic!("settings resolve before locator construction: {error}"));
    let error = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &profile,
        &resolved_settings,
        &WorkflowDocument::default(),
        &[],
        None,
    )
    .expect_err("invalid provenance must abort packet construction");
    assert!(error.to_string().contains("provenance source locator"));

    // A corrupted retained source digest is equally fatal.
    let broken_document = WorkflowDocument {
        sources: vec![WorkflowSourceObservation {
            role: RepositoryOverride,
            path: PathBuf::from("/repo/WORKFLOW.md"),
            content_sha256: "not-a-digest".to_string(),
        }],
        ..Default::default()
    };
    let profile = codex_profile();
    let error = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &profile,
        &resolved(&profile, None),
        &broken_document,
        &[],
        None,
    )
    .expect_err("invalid retained source digest must abort packet construction");
    assert!(error.to_string().contains("content digest"));
}

#[test]
fn model_facing_prompt_strips_audit_sections_but_durable_packet_keeps_them() {
    let mut job = runtime_job("implement_prompt");
    job.input = json!({
        "activity": "implement_prompt",
        "command": { "prompt_ref": "prompt-1" },
    });
    let profile = codex_profile();
    let task_text = "Implement the requested change.";
    let packet = build_packet(
        &job,
        &profile,
        &WorkflowDocument::default(),
        &[],
        Some(task_text),
    );

    assert!(packet.get("context_provenance").is_some());
    assert!(packet.get("resolved_runtime_settings").is_some());
    assert!(packet.get("prompt_task_request").is_some());

    let prompt = build_runtime_job_prompt(&packet, Some(task_text));
    assert!(!prompt.contains("context_provenance"));
    assert!(!prompt.contains("resolved_runtime_settings"));
    assert!(!prompt.contains("prompt_task_request"));
    // The prompt-task text still reaches the agent exactly as before.
    assert!(prompt.contains(task_text));
}
