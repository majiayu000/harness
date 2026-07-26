//! Runtime context provenance for workflow-runtime prompt packets (GH-1732).
//!
//! Records which behavior-affecting inputs Harness actually selected while
//! constructing one prompt packet, using the canonical ASC-001 component
//! model from `harness_core::stack`. Repository discovery alone never
//! produces an entry: every entry corresponds to an input this process
//! resolved or loaded for the packet being built.
//!
//! ASC-001 locator grammar requires snake_case namespaces and rejects
//! UUID-shaped segments, so locators use `runtime_profile/...`,
//! `workflow_source/...`, `workflow_document/...`, and
//! `repo_memory/record-<uuid>` forms that validate against that contract.

use anyhow::Context;
use harness_core::config::workflow::{
    WorkflowConfig, WorkflowDocument, WorkflowSourceObservation, WorkflowSourceRole,
};
use harness_core::stack::{
    AgentStackComponent, AgentStackComponentKind, AgentStackFreshness, AgentStackObservationClass,
    AgentStackSelectionState, AgentStackSource, AgentStackSourceScope, AgentStackTrustLevel,
    Sha256Digest,
};
use harness_workflow::runtime::{RetrievedRepoMemoryRecord, RuntimeJob};
use serde::Serialize;
use serde_json::{json, Value};

use super::{ResolvedRuntimeSettings, REPO_MEMORY_PROMPT_PREAMBLE, RUNTIME_PROMPT_PACKET_SCHEMA};

pub(super) const CONTEXT_PROVENANCE_SCHEMA: &str = "harness.runtime.context_provenance.v1";
const HISTORICAL_PROMPT_PACKET_SCHEMA_V1: &str = "harness.runtime.prompt_packet.v1";

/// Closed coverage markers for context Harness cannot observe. Absence of a
/// manifest entry is never proof that such context did not exist.
const NOT_OBSERVED_BY_HARNESS: [&str; 4] = [
    "agent_cli_context_not_observed",
    "mcp_host_context_not_observed",
    "user_global_context_not_observed",
    "model_provider_context_not_observed",
];

const REASON_RUNTIME_PROFILE_SELECTED: &str = "workflow_runtime_profile_selected";
const REASON_WORKFLOW_BASE_SELECTED: &str = "workflow_base_selected";
const REASON_WORKFLOW_REPOSITORY_SELECTED: &str = "workflow_repository_selected";
const REASON_WORKFLOW_DOCUMENT_EFFECTIVE: &str = "workflow_document_effective";
const REASON_WORKFLOW_DEFAULTS_SELECTED: &str = "workflow_defaults_selected";
const REASON_REPO_MEMORY_SELECTED: &str = "repo_memory_selected";

/// Serializable provenance envelope nested in the prompt packet.
#[derive(Debug, Serialize)]
pub(super) struct ContextProvenance {
    schema: &'static str,
    entries: Vec<ContextProvenanceEntry>,
    not_observed_by_harness: [&'static str; 4],
}

#[derive(Debug, Serialize)]
struct ContextProvenanceEntry {
    order: usize,
    reason: &'static str,
    component: AgentStackComponent,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_metadata: Option<MemoryEntryMetadata>,
}

/// Safe metadata extension for selected repo-memory records. Never contains
/// raw payload content; the payload stays only in the packet memory section.
#[derive(Debug, Serialize)]
struct MemoryEntryMetadata {
    record_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_ref: Option<String>,
    estimated_tokens: usize,
}

/// Canonical digest input shared by the effective-document and defaults
/// entries: deterministic field order, no unordered maps.
#[derive(Serialize)]
struct CanonicalWorkflowDocumentDigestInput<'a> {
    config: &'a WorkflowConfig,
    prompt_template: &'a str,
}

/// Build provenance and the audit-only packet sections, then validate the
/// finished packet. Any failure aborts prompt preparation before the packet
/// is hashed, recorded, or executed; there is no empty-manifest fallback.
pub(super) fn apply_context_provenance(
    packet: &mut Value,
    job: &RuntimeJob,
    resolved_settings: &ResolvedRuntimeSettings,
    workflow_document: &WorkflowDocument,
    repo_memory: &[RetrievedRepoMemoryRecord],
    prompt_task_text: Option<&str>,
) -> anyhow::Result<()> {
    let provenance = build_context_provenance(resolved_settings, workflow_document, repo_memory)?;
    packet["resolved_runtime_settings"] = serde_json::to_value(resolved_settings)
        .context("failed to serialize resolved runtime settings for the prompt packet")?;
    packet["context_provenance"] = serde_json::to_value(&provenance)
        .context("failed to serialize required context provenance for the prompt packet")?;
    if let Some(task_text) = prompt_task_text {
        packet["prompt_task_request"] = prompt_task_request_section(job, task_text)?;
    }
    validate_prompt_packet_provenance(packet)
}

/// Redacted prompt-task binding: durable reference plus SHA-256 of the exact
/// UTF-8 task text, hashed before the enclosing packet digest. Raw task text
/// never enters the packet or provenance.
fn prompt_task_request_section(job: &RuntimeJob, task_text: &str) -> anyhow::Result<Value> {
    let prompt_ref = job
        .input
        .pointer("/command/prompt_ref")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime prompt task text was resolved without a durable prompt_ref")
        })?;
    Ok(json!({
        "prompt_ref": prompt_ref,
        "task_text_sha256": Sha256Digest::from_bytes(task_text.as_bytes()).as_str(),
    }))
}

/// Require provenance for v2 packets while accepting historical v1 packets
/// as lower-evidence records that are never interpreted as v2.
pub(super) fn validate_prompt_packet_provenance(packet: &Value) -> anyhow::Result<()> {
    let schema = packet.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != RUNTIME_PROMPT_PACKET_SCHEMA {
        if schema == HISTORICAL_PROMPT_PACKET_SCHEMA_V1 {
            return Ok(());
        }
        anyhow::bail!("prompt packet schema `{schema}` is not a supported runtime packet schema");
    }
    let Some(provenance) = packet.get("context_provenance") else {
        anyhow::bail!("a v2 prompt packet requires a context_provenance object");
    };
    let provenance_schema = provenance
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("");
    if provenance_schema != CONTEXT_PROVENANCE_SCHEMA {
        anyhow::bail!(
            "prompt packet context provenance schema `{provenance_schema}` is not supported"
        );
    }
    if provenance
        .get("entries")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        anyhow::bail!("a v2 prompt packet requires at least one context provenance entry");
    }
    Ok(())
}

/// Remove audit-only sections from the model-facing packet clone so the
/// agent-visible prompt bytes for unchanged inputs match the v1 rendering.
pub(super) fn strip_model_facing_audit_sections(model_packet: &mut Value) {
    if let Some(object) = model_packet.as_object_mut() {
        object.remove("context_provenance");
        object.remove("resolved_runtime_settings");
        object.remove("prompt_task_request");
    }
}

fn build_context_provenance(
    resolved_settings: &ResolvedRuntimeSettings,
    workflow_document: &WorkflowDocument,
    repo_memory: &[RetrievedRepoMemoryRecord],
) -> anyhow::Result<ContextProvenance> {
    let mut entries = Vec::new();
    entries.push(resolved_runtime_settings_entry(
        resolved_settings,
        entries.len(),
    )?);
    append_workflow_entries(&mut entries, workflow_document)?;
    for record in repo_memory {
        entries.push(repo_memory_entry(record, entries.len())?);
    }
    Ok(ContextProvenance {
        schema: CONTEXT_PROVENANCE_SCHEMA,
        entries,
        not_observed_by_harness: NOT_OBSERVED_BY_HARNESS,
    })
}

fn resolved_runtime_settings_entry(
    resolved_settings: &ResolvedRuntimeSettings,
    order: usize,
) -> anyhow::Result<ContextProvenanceEntry> {
    let digest = Sha256Digest::from_bytes(
        &serde_json::to_vec(resolved_settings)
            .context("failed to serialize resolved runtime settings for provenance hashing")?,
    );
    let source = AgentStackSource::new(
        AgentStackSourceScope::Runtime,
        &format!("runtime_profile/{}", resolved_settings.profile_name),
    )
    .with_context(|| {
        format!(
            "runtime profile name `{}` cannot form a valid provenance source locator",
            resolved_settings.profile_name
        )
    })?;
    provenance_entry(
        AgentStackComponentKind::AgentRuntime,
        source,
        AgentStackTrustLevel::RuntimeObserved,
        digest,
        REASON_RUNTIME_PROFILE_SELECTED,
        order,
        None,
    )
}

fn append_workflow_entries(
    entries: &mut Vec<ContextProvenanceEntry>,
    workflow_document: &WorkflowDocument,
) -> anyhow::Result<()> {
    for source in &workflow_document.sources {
        entries.push(workflow_source_entry(source, entries.len())?);
    }
    if workflow_document.sources.is_empty() {
        entries.push(workflow_document_entry(
            "workflow_document/defaults",
            REASON_WORKFLOW_DEFAULTS_SELECTED,
            &WorkflowConfig::default(),
            "",
            entries.len(),
        )?);
    } else {
        entries.push(workflow_document_entry(
            "workflow_document/effective",
            REASON_WORKFLOW_DOCUMENT_EFFECTIVE,
            &workflow_document.config,
            &workflow_document.prompt_template,
            entries.len(),
        )?);
    }
    Ok(())
}

/// One configured workflow source. Central-base paths can be unsafe absolute
/// paths outside the repository, so they are represented only by a
/// runtime-scoped digest of the canonical path; the repository override uses
/// the normalized `WORKFLOW.md` repository locator. Classification comes from
/// the retained source facts, never from display-path normalization.
fn workflow_source_entry(
    source: &WorkflowSourceObservation,
    order: usize,
) -> anyhow::Result<ContextProvenanceEntry> {
    let content_digest = Sha256Digest::parse(&source.content_sha256)
        .context("workflow source observation carried an invalid content digest")?;
    let (stack_source, reason) = match source.role {
        WorkflowSourceRole::CentralBase => {
            let path_digest =
                Sha256Digest::from_bytes(source.path.display().to_string().as_bytes());
            (
                AgentStackSource::new(
                    AgentStackSourceScope::Runtime,
                    &format!("workflow_source/central/{}", path_digest.as_str()),
                )
                .context("central workflow source locator failed validation")?,
                REASON_WORKFLOW_BASE_SELECTED,
            )
        }
        WorkflowSourceRole::RepositoryOverride => (
            AgentStackSource::new(AgentStackSourceScope::Repository, "WORKFLOW.md")
                .context("repository workflow source locator failed validation")?,
            REASON_WORKFLOW_REPOSITORY_SELECTED,
        ),
    };
    provenance_entry(
        AgentStackComponentKind::Workflow,
        stack_source,
        AgentStackTrustLevel::RuntimeObserved,
        content_digest,
        reason,
        order,
        None,
    )
}

/// The normalized effective document (or explicit runtime defaults when no
/// configured source exists). Both digest the same canonical JSON shape, so
/// conforming producers emit identical defaults digests and any change to
/// behavior-affecting defaults changes the digest.
fn workflow_document_entry(
    locator: &str,
    reason: &'static str,
    config: &WorkflowConfig,
    prompt_template: &str,
    order: usize,
) -> anyhow::Result<ContextProvenanceEntry> {
    let digest = Sha256Digest::from_bytes(
        &serde_json::to_vec(&CanonicalWorkflowDocumentDigestInput {
            config,
            prompt_template,
        })
        .context("failed to serialize the workflow document for provenance hashing")?,
    );
    let source = AgentStackSource::new(AgentStackSourceScope::Runtime, locator)
        .context("workflow document locator failed validation")?;
    provenance_entry(
        AgentStackComponentKind::Workflow,
        source,
        AgentStackTrustLevel::RuntimeObserved,
        digest,
        reason,
        order,
        None,
    )
}

/// One selected repo-memory record. Harness observed the selection, but the
/// memory claim itself stays `self_declared` because selection does not
/// validate it. The digest covers the exact redacted packet representation of
/// this record; the payload itself is not duplicated into provenance.
fn repo_memory_entry(
    record: &RetrievedRepoMemoryRecord,
    order: usize,
) -> anyhow::Result<ContextProvenanceEntry> {
    let digest = Sha256Digest::from_bytes(
        &serde_json::to_vec(&repo_memory_record_value(record))
            .context("failed to serialize a selected repo-memory record for provenance hashing")?,
    );
    let source = AgentStackSource::new(
        AgentStackSourceScope::Runtime,
        &format!("repo_memory/record-{}", record.record.id),
    )
    .context("repo-memory record locator failed validation")?;
    provenance_entry(
        AgentStackComponentKind::Memory,
        source,
        AgentStackTrustLevel::SelfDeclared,
        digest,
        REASON_REPO_MEMORY_SELECTED,
        order,
        Some(MemoryEntryMetadata {
            record_id: record.record.id.to_string(),
            evidence_ref: record.record.evidence_ref.clone(),
            estimated_tokens: record.estimated_tokens,
        }),
    )
}

fn provenance_entry(
    kind: AgentStackComponentKind,
    source: AgentStackSource,
    trust_level: AgentStackTrustLevel,
    digest: Sha256Digest,
    reason: &'static str,
    order: usize,
    memory_metadata: Option<MemoryEntryMetadata>,
) -> anyhow::Result<ContextProvenanceEntry> {
    let component = AgentStackComponent::new(
        kind,
        source,
        AgentStackObservationClass::RuntimeObserved,
        AgentStackSelectionState::Loaded,
        trust_level,
        AgentStackFreshness::Fresh,
    )
    .context("provenance component construction failed ASC-001 validation")?
    .with_integrity(Some(digest));
    component
        .validate()
        .context("provenance component failed ASC-001 validation")?;
    Ok(ContextProvenanceEntry {
        order,
        reason,
        component,
        memory_metadata,
    })
}

pub(super) fn repo_memory_prompt_value(repo_memory: &[RetrievedRepoMemoryRecord]) -> Value {
    json!({
        "schema": "harness.runtime.repo_memory.v1",
        "preamble": REPO_MEMORY_PROMPT_PREAMBLE,
        "records": repo_memory
            .iter()
            .map(repo_memory_record_value)
            .collect::<Vec<_>>()
    })
}

/// The exact redacted packet representation of one selected memory record;
/// also the digest input for that record's provenance entry.
fn repo_memory_record_value(entry: &RetrievedRepoMemoryRecord) -> Value {
    let record = &entry.record;
    json!({
        "id": record.id.to_string(),
        "repo": &record.repo,
        "activity_class": &record.activity_class,
        "outcome": record.outcome.db_value(),
        "kind": record.kind.db_value(),
        "estimated_tokens": entry.estimated_tokens,
        "evidence_ref": &record.evidence_ref,
        "created_at": record.created_at.to_rfc3339(),
        "use_count": record.use_count,
        "payload": &record.payload_json,
    })
}

pub(super) fn repo_memory_prompt_section(prompt_packet: &Value) -> Option<String> {
    let repo_memory = prompt_packet.get("repo_memory")?;
    if repo_memory
        .get("records")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return None;
    }
    Some(format!(
        "\nRepo memory:\n```repo-memory\n{}\n{}\n```\n",
        REPO_MEMORY_PROMPT_PREAMBLE,
        super::pretty_json(repo_memory)
    ))
}

#[cfg(test)]
#[path = "context_provenance_tests.rs"]
mod context_provenance_tests;
