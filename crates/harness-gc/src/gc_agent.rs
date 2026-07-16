use crate::checkpoint::{filter_events_since, GcCheckpoint};
use crate::draft_store::DraftStore;
use crate::remediation::signal_priority;
use crate::signal_detector::SignalDetector;
use anyhow::Context;
use chrono::{DateTime, Utc};
use harness_core::agent::{AgentRequest, CodeAgent};
use harness_core::config::{
    agents::CapabilityProfile,
    misc::{AutoAdoptPolicy, GcConfig},
};
use harness_core::types::{
    Artifact, ArtifactType, Draft, DraftId, DraftStatus, ExternalSignal, Project, RemediationType,
    Signal, SignalType,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Default tools allowed during GC agent execution.
const DEFAULT_GC_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcReport {
    pub signals: Vec<Signal>,
    pub drafts_generated: usize,
    /// IDs of drafts created during this run. Used by auto-adoption paths that
    /// need to identify freshly-created drafts without re-querying the store.
    #[serde(default)]
    pub draft_ids: Vec<DraftId>,
    pub errors: Vec<String>,
}

pub struct GcAgent {
    config: GcConfig,
    signal_detector: SignalDetector,
    draft_store: DraftStore,
    /// Canonical project root used to validate artifact target paths in `adopt`.
    project_root: PathBuf,
    /// Path to the checkpoint file; `None` disables checkpoint-based scanning.
    checkpoint_path: Option<PathBuf>,
}

impl GcAgent {
    pub fn new(
        config: GcConfig,
        signal_detector: SignalDetector,
        draft_store: DraftStore,
        project_root: PathBuf,
    ) -> Self {
        Self {
            config,
            signal_detector,
            draft_store,
            project_root,
            checkpoint_path: None,
        }
    }

    /// Configure the checkpoint file path used for incremental scanning.
    pub fn with_checkpoint(mut self, path: PathBuf) -> Self {
        self.checkpoint_path = Some(path);
        self
    }

    /// Return the `last_scan_at` timestamp from the persisted checkpoint, if any.
    ///
    /// Callers can use this to restrict a DB event query to the incremental
    /// window (`EventFilters { since: gc_agent.checkpoint_since(), .. }`) so
    /// that the DB applies the row restriction before materializing results,
    /// rather than loading the entire event history into memory first.
    pub fn checkpoint_since(&self) -> Option<DateTime<Utc>> {
        self.checkpoint_path
            .as_deref()
            .and_then(GcCheckpoint::load)
            .map(|cp| cp.last_scan_at)
    }

    /// Return the list of files changed since the last checkpoint commit.
    ///
    /// Host-side git inspection is disabled by project policy, so GC uses a
    /// full scan until changed-file signals are supplied by the orchestrating
    /// agent or event stream.
    pub async fn changed_files_since_checkpoint(&self) -> Vec<PathBuf> {
        tracing::info!(
            project_root = %self.project_root.display(),
            "gc: full scan: host-side git changed-file detection disabled"
        );
        Vec::new()
    }

    /// Run a GC cycle using incremental scanning when a checkpoint is configured.
    ///
    /// If a valid checkpoint exists only events since `last_scan_at` are analysed.
    /// Falls back to a full scan when the checkpoint is missing or corrupt.
    /// On success the checkpoint is updated with the current timestamp and HEAD commit.
    pub async fn run(
        &self,
        project: &Project,
        events: &[harness_core::types::Event],
        violations: &[harness_core::types::Violation],
        agent: &dyn CodeAgent,
    ) -> anyhow::Result<GcReport> {
        self.run_with_external(project, events, violations, &[], agent)
            .await
    }

    /// Run a GC cycle with optional external signals merged into detection.
    pub async fn run_with_external(
        &self,
        project: &Project,
        events: &[harness_core::types::Event],
        violations: &[harness_core::types::Violation],
        external_signals: &[ExternalSignal],
        agent: &dyn CodeAgent,
    ) -> anyhow::Result<GcReport> {
        // 0. Expire stale drafts before generating new ones.
        self.draft_store
            .expire_stale_drafts(self.config.draft_ttl_hours)?;

        // Determine incremental cutoff from checkpoint (None → full scan).
        let since = self
            .checkpoint_path
            .as_deref()
            .and_then(GcCheckpoint::load)
            .map(|cp| cp.last_scan_at);

        // Log scan mode via git-diff helper.
        let _changed = self.changed_files_since_checkpoint().await;

        let scanned_events = filter_events_since(events, since);

        // 1. Detect signals from events (including persisted `rule_check` violations).
        let mut signals = self.signal_detector.detect(scanned_events);
        // Back-compat: if the caller provided live violations but they haven't been
        // persisted into `events` yet, fall back to the old detector.
        if !violations.is_empty() && !events.iter().any(|e| e.hook == "rule_check") {
            signals.extend(self.signal_detector.from_violations(violations));
        }
        if !external_signals.is_empty() {
            signals.extend(self.signal_detector.detect_from_external(external_signals));
        }

        // 2. Prioritize.
        signals.sort_by_key(|s| signal_priority(s.signal_type));

        // 3. Generate fix drafts (up to max_drafts_per_run).
        //
        // Two-phase design for cancellation safety:
        //   Phase A – invoke agents and persist each draft immediately after generation.
        //             Contains .await points (agent execution + pre-fetched HEAD).
        //   Phase B – update checkpoint atomically (sync, zero .await points).
        //
        // Each draft is written to the store as soon as it is produced, so a
        // tokio::time::timeout cancellation mid-loop cannot discard work that
        // completed for earlier signals.
        //
        // HEAD is fetched before the loop so that Phase B (checkpoint write) has no
        // .await, eliminating the window where drafts are on disk but the checkpoint
        // is stale.
        let mut errors = Vec::new();
        let allowed_tools: Vec<String> = self
            .config
            .allowed_tools
            .clone()
            .unwrap_or_else(|| DEFAULT_GC_TOOLS.iter().map(|s| s.to_string()).collect());

        // Pre-fetch HEAD before any writes so Phase B (checkpoint) needs no .await.
        let head_commit = if self.checkpoint_path.is_some() {
            get_current_head(&self.project_root).await
        } else {
            None
        };

        // Phase A: run agent for each signal; persist each draft immediately.
        let mut draft_ids: Vec<DraftId> = Vec::new();
        let mut drafts_generated = 0;
        for signal in signals.iter().take(self.config.max_drafts_per_run) {
            let base_prompt = build_prompt(signal, project);
            // Inject capability restriction note as secondary context alongside
            // --allowedTools CLI enforcement (issue #514).
            let prompt = if let Some(note) = CapabilityProfile::ReadOnly.prompt_note() {
                format!("{note}\n\n{base_prompt}")
            } else {
                base_prompt
            };

            let result = agent
                .execute(AgentRequest {
                    prompt,
                    project_root: project.root.clone(),
                    allowed_tools: Some(allowed_tools.clone()),
                    max_budget_usd: Some(self.config.budget_per_signal_usd),
                    ..Default::default()
                })
                .await;

            match result {
                Ok(resp) => {
                    let draft = Draft {
                        id: DraftId::new(),
                        status: DraftStatus::Pending,
                        signal: signal.clone(),
                        artifacts: parse_artifacts(&resp.output, signal),
                        rationale: format!(
                            "Auto-generated fix for {:?} signal",
                            signal.signal_type
                        ),
                        validation: "Run guard check after applying".to_string(),
                        generated_at: Utc::now(),
                        agent_model: resp.model,
                    };
                    // Persist immediately so a future cancellation (timeout) cannot
                    // discard work already completed for this signal.
                    if let Err(e) = self.draft_store.save(&draft) {
                        errors.push(format!("failed to save draft: {e}"));
                    } else {
                        drafts_generated += 1;
                        draft_ids.push(draft.id);
                    }
                }
                Err(e) => {
                    errors.push(format!(
                        "agent failed for signal {:?}: {e}",
                        signal.signal_type
                    ));
                }
            }
        }

        let report = GcReport {
            signals,
            drafts_generated,
            draft_ids,
            errors,
        };

        // Phase B: update checkpoint (sync — no .await points).
        // HEAD was pre-fetched before the loop, so this block is cancellation-safe.
        if let Some(path) = &self.checkpoint_path {
            let mut cp = GcCheckpoint::new(Utc::now());
            if let Some(h) = head_commit {
                cp = cp.with_head_commit(h);
            }
            if let Err(e) = cp.save(path) {
                tracing::warn!("failed to save gc checkpoint: {e}");
            }
        }

        Ok(report)
    }

    /// Adopt a draft: write artifacts to disk.
    ///
    /// All artifact target_paths are canonicalized and validated to reside
    /// within the project root before any writes occur.
    ///
    /// The status check, file writes, and status commit are all performed under
    /// `DraftStore::transition_lock` to prevent a concurrent `reject()` from
    /// winning the race between file writes and status commit.
    pub fn adopt(&self, draft_id: &DraftId) -> anyhow::Result<()> {
        let canonical_root = self.project_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize project root '{}'",
                self.project_root.display()
            )
        })?;

        self.draft_store.adopt_if_pending(draft_id, |draft| {
            // Validate and resolve all paths before writing any files.
            let resolved: Vec<PathBuf> = draft
                .artifacts
                .iter()
                .map(|a| validate_target_path(&canonical_root, &a.target_path))
                .collect::<anyhow::Result<_>>()?;

            for (artifact, target) in draft.artifacts.iter().zip(resolved.iter()) {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, &artifact.content)?;
            }
            Ok(())
        })
    }

    /// Auto-adopt drafts from `draft_ids` that are eligible under `policy`.
    ///
    /// For `AutoAdoptPolicy::RulesOnly`, a draft is eligible when:
    ///   1. its signal remediation is `RemediationType::Rule`, AND
    ///   2. every artifact `target_path` (after lexical normalization) is a
    ///      relative path whose first component(s) match `path_prefix`.
    ///
    /// Returns the list of draft IDs that were successfully adopted. Failures
    /// to adopt individual drafts are logged and do not abort the loop.
    ///
    /// This method is a no-op when `policy` is `Off` or `draft_ids` is empty.
    pub fn auto_adopt_matching(
        &self,
        draft_ids: &[DraftId],
        policy: AutoAdoptPolicy,
        path_prefix: &str,
    ) -> Vec<DraftId> {
        // Exhaustive match: adding a new AutoAdoptPolicy variant must become a
        // compile error here, not a silent no-op that drops all drafts.
        match policy {
            AutoAdoptPolicy::Off => return Vec::new(),
            AutoAdoptPolicy::RulesOnly => {}
        }
        if draft_ids.is_empty() {
            return Vec::new();
        }
        let mut adopted = Vec::new();
        for id in draft_ids {
            let draft = match self.draft_store.get(id) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(draft_id = %id.0, error = %e, "auto_adopt: failed to load draft");
                    continue;
                }
            };
            if draft.status != DraftStatus::Pending {
                tracing::debug!(draft_id = %id.0, status = ?draft.status, "auto_adopt: skipping non-Pending draft");
                continue;
            }
            if draft.signal.remediation != RemediationType::Rule {
                continue;
            }
            if draft.artifacts.is_empty() {
                continue;
            }
            let all_under_prefix = draft
                .artifacts
                .iter()
                .all(|a| artifact_path_under_prefix(&a.target_path, path_prefix));
            if !all_under_prefix {
                continue;
            }
            match self.adopt(id) {
                Ok(()) => adopted.push(id.clone()),
                Err(e) => tracing::warn!(
                    draft_id = %id.0,
                    error = %e,
                    "auto_adopt: adopt failed"
                ),
            }
        }
        adopted
    }

    /// Reject a draft.
    ///
    /// Uses `save_if_status` to prevent an unconditional overwrite of an already-Adopted
    /// draft when `adopt()` and `reject()` race on the same draft.
    pub fn reject(&self, draft_id: &DraftId, _reason: Option<&str>) -> anyhow::Result<()> {
        let mut draft = self
            .draft_store
            .get(draft_id)?
            .ok_or_else(|| anyhow::anyhow!("draft not found"))?;
        draft.status = DraftStatus::Rejected;
        self.draft_store
            .save_if_status(&draft, DraftStatus::Pending)?;
        Ok(())
    }

    pub fn drafts(&self) -> anyhow::Result<Vec<Draft>> {
        self.draft_store.list()
    }

    pub fn draft_store(&self) -> &DraftStore {
        &self.draft_store
    }
}

/// Return the current revision identifier when it is provided by a future
/// agent-owned signal. Host-side git inspection is disabled, so this currently
/// records no commit hash.
async fn get_current_head(project_root: &Path) -> Option<String> {
    let _ = project_root;
    None
}

/// Validate and resolve an artifact target path to an absolute path within `project_root`.
///
/// Steps:
/// 1. Make the path absolute by joining it with `project_root` (if relative).
/// 2. Lexically normalize `.` and `..` components.
/// 3. Resolve the nearest existing ancestor via `canonicalize` to detect symlink escapes.
/// 4. Verify the resolved path starts with `project_root`.
fn validate_target_path(project_root: &Path, target_path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        project_root.join(target_path)
    };
    let normalized = normalize_path(&absolute);
    let resolved = resolve_for_boundary_check(&normalized)?;

    if resolved == project_root || resolved.starts_with(project_root) {
        return Ok(normalized);
    }

    anyhow::bail!(
        "resolved path '{}' is outside project root '{}'",
        resolved.display(),
        project_root.display()
    );
}

/// Lexically normalize a path: collapse `.` and `..` without hitting the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    out
}

/// Returns true when `target_path` is a relative path that, after lexical
/// normalization, lies under `path_prefix`. Used by auto-adoption to limit
/// writes to a configured sandbox directory (e.g. `.harness/generated/`).
///
/// Absolute paths and paths that escape the prefix (`..`) return false.
fn artifact_path_under_prefix(target_path: &Path, path_prefix: &str) -> bool {
    if target_path.is_absolute() {
        return false;
    }
    let normalized = normalize_path(target_path);
    // Reject paths that still contain ParentDir segments after normalization
    // (e.g. `..` at the front, which normalize_path leaves intact when the
    // output is empty).
    if normalized
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return false;
    }
    let prefix_normalized = normalize_path(Path::new(path_prefix));
    normalized.starts_with(&prefix_normalized)
}

/// Walk up from `path` to find the nearest existing ancestor, canonicalize it,
/// then re-attach the remaining suffix. This allows boundary checks on paths
/// that do not yet exist on disk.
fn resolve_for_boundary_check(path: &Path) -> anyhow::Result<PathBuf> {
    let mut ancestor = path;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            anyhow::anyhow!("no existing ancestor found for '{}'", path.display())
        })?;
    }
    let canonical = ancestor.canonicalize()?;
    let suffix = path
        .strip_prefix(ancestor)
        .map_err(|_| anyhow::anyhow!("failed to compute suffix for '{}'", path.display()))?;
    Ok(canonical.join(suffix))
}

fn build_prompt(signal: &Signal, project: &Project) -> String {
    use harness_core::prompts::gc_signal;
    let project_name = project.name.as_str();
    // `Signal::details` is a `serde_json::Value`; rendering it via `Display`
    // preserves the JSON shape that the original inline `format!("{}", ...)`
    // produced (e.g. quoted string variants).
    let details = signal.details.to_string();
    match signal.signal_type {
        SignalType::RepeatedWarn => gc_signal::repeated_warn(project_name, &details),
        SignalType::ChronicBlock => gc_signal::chronic_block(project_name, &details),
        SignalType::HotFiles => gc_signal::hot_files(project_name, &details),
        SignalType::SlowSessions => gc_signal::slow_sessions(project_name, &details),
        SignalType::WarnEscalation => gc_signal::warn_escalation(project_name, &details),
        SignalType::LinterViolations => gc_signal::linter_violations(project_name, &details),
    }
}

fn parse_artifacts(output: &str, signal: &Signal) -> Vec<Artifact> {
    let artifact_type = match signal.remediation {
        RemediationType::Guard => ArtifactType::Guard,
        RemediationType::Rule => ArtifactType::Rule,
        RemediationType::Hook => ArtifactType::Hook,
        RemediationType::Skill => ArtifactType::Skill,
    };

    let code_artifacts = extract_code_block_artifacts(output, artifact_type);
    let code_paths: std::collections::HashSet<String> = code_artifacts
        .iter()
        .map(|a| a.target_path.to_string_lossy().into_owned())
        .collect();
    let diff_artifacts =
        extract_diff_artifacts(output, artifact_type, signal.id.as_str(), &code_paths);

    // Merge: code blocks take precedence over diffs for the same path
    let mut seen = std::collections::HashSet::new();
    let mut artifacts: Vec<Artifact> = Vec::new();
    for artifact in code_artifacts.into_iter().chain(diff_artifacts) {
        if seen.insert(artifact.target_path.clone()) {
            artifacts.push(artifact);
        }
    }

    // Fall back to single blob if no structured content found
    if artifacts.is_empty() {
        artifacts.push(Artifact {
            artifact_type,
            target_path: std::path::PathBuf::from(format!(".harness/drafts/{}.md", signal.id)),
            content: output.to_string(),
        });
    }

    artifacts
}

/// Extract artifacts from fenced code blocks that include a file path.
///
/// Matches patterns like:
/// ` ```rust path/to/file.rs `
/// ` ```python src/foo.py `
fn extract_code_block_artifacts(output: &str, artifact_type: ArtifactType) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("```") {
            // Split into at most 2 parts: language and path
            let mut parts = rest.splitn(2, ' ');
            let _lang = parts.next().unwrap_or("");
            if let Some(path) = parts.next() {
                let path = path.trim();
                if is_valid_file_path(path) {
                    // Collect block content until closing ```
                    let start = i + 1;
                    let mut end = start;
                    while end < lines.len() && lines[end] != "```" {
                        end += 1;
                    }
                    artifacts.push(Artifact {
                        artifact_type,
                        target_path: std::path::PathBuf::from(path),
                        content: lines[start..end].join("\n"),
                    });
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }

    artifacts
}

/// Extract artifacts from unified diff hunks.
///
/// Matches `--- a/path` / `+++ b/path` headers and collects all following
/// hunk lines (`@@` and context/add/remove lines).
fn extract_diff_artifacts(
    output: &str,
    artifact_type: ArtifactType,
    signal_id: &str,
    code_paths: &std::collections::HashSet<String>,
) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;
    let mut diff_idx: usize = 0;

    while i < lines.len() {
        if lines[i].starts_with("--- ") && i + 1 < lines.len() && lines[i + 1].starts_with("+++ ") {
            let plus_line = lines[i + 1];
            let raw_path = plus_line
                .strip_prefix("+++ b/")
                .or_else(|| plus_line.strip_prefix("+++ "))
                .unwrap_or("")
                .trim();

            if !raw_path.is_empty() && raw_path != "/dev/null" && is_valid_file_path(raw_path) {
                if code_paths.contains(raw_path) {
                    i += 2;
                    continue;
                }
                let start = i;
                let mut end = i + 2;
                // Collect hunk lines; stop at the next diff header
                while end < lines.len() {
                    if lines[end].starts_with("--- ")
                        && end + 1 < lines.len()
                        && lines[end + 1].starts_with("+++ ")
                    {
                        break;
                    }
                    end += 1;
                }
                diff_idx = diff_idx.saturating_add(1);
                let patch_name = format!("{signal_id}-{diff_idx}.patch");
                artifacts.push(Artifact {
                    artifact_type,
                    target_path: std::path::PathBuf::from(".harness/drafts").join(patch_name),
                    content: lines[start..end].join("\n"),
                });
                i = end;
                continue;
            }
        }
        i += 1;
    }

    artifacts
}

/// Returns true when `s` looks like a file path (contains `/` or `.`).
fn is_valid_file_path(s: &str) -> bool {
    !s.is_empty() && (s.contains('/') || s.contains('.'))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests;
