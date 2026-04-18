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
    /// Reads the `head_commit` from the checkpoint, runs
    /// `git diff --name-only <hash>..HEAD`, and returns the changed paths.
    /// Returns an empty vec (triggering a full scan) when:
    /// - no checkpoint is configured,
    /// - the checkpoint has no `head_commit` recorded, or
    /// - `git diff` fails.
    ///
    /// Also emits a tracing log line describing the scan mode.
    pub async fn changed_files_since_checkpoint(&self) -> Vec<PathBuf> {
        let Some(cp_path) = &self.checkpoint_path else {
            tracing::info!("gc: full scan: no checkpoint found");
            return vec![];
        };
        let Some(cp) = GcCheckpoint::load(cp_path) else {
            tracing::info!("gc: full scan: no checkpoint found");
            return vec![];
        };
        let Some(base_commit) = cp.head_commit else {
            tracing::info!("gc: full scan: no checkpoint found");
            return vec![];
        };

        let range = format!("{base_commit}..HEAD");
        match tokio::process::Command::new("git")
            .args(["diff", "--name-only", &range])
            .current_dir(&self.project_root)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                let files: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect();
                tracing::info!(
                    "gc: incremental scan: {} files changed since last checkpoint",
                    files.len()
                );
                files
            }
            Ok(out) => {
                tracing::warn!(
                    "gc: git diff failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                tracing::info!("gc: full scan: falling back after git diff error");
                vec![]
            }
            Err(e) => {
                tracing::warn!("gc: git diff error: {e}");
                tracing::info!("gc: full scan: falling back after git diff error");
                vec![]
            }
        }
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

/// Run `git rev-parse HEAD` in `project_root` and return the commit hash, or `None` on failure.
async fn get_current_head(project_root: &Path) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .await
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
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
    match signal.signal_type {
        SignalType::RepeatedWarn => format!(
            "Analyze repeated warnings in project {} and generate a guard script to detect this pattern.\nDetails: {}",
            project.name,
            signal.details
        ),
        SignalType::ChronicBlock => format!(
            "Analyze chronic block events in project {} and suggest rule improvements to reduce false blocks.\nDetails: {}",
            project.name,
            signal.details
        ),
        SignalType::HotFiles => format!(
            "Analyze frequently edited files in project {} and create a SKILL.md with editing strategies.\nDetails: {}",
            project.name,
            signal.details
        ),
        SignalType::SlowSessions => format!(
            "Analyze slow operations in project {} and create a performance optimization SKILL.md.\nDetails: {}",
            project.name,
            signal.details
        ),
        SignalType::WarnEscalation => format!(
            "Warning trends are escalating in project {}. Suggest rule upgrades (warn → block).\nDetails: {}",
            project.name,
            signal.details
        ),
        SignalType::LinterViolations => format!(
            "Code scan found violations in project {}. Generate a guard script to detect and prevent these.\nDetails: {}",
            project.name,
            signal.details
        ),
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
mod tests {
    use super::*;
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftStatus, ProjectId, RemediationType, Signal, SignalType,
    };

    fn make_test_gc_agent(dir: &std::path::Path) -> GcAgent {
        let signal_detector = SignalDetector::new(
            crate::signal_detector::SignalThresholds::default(),
            ProjectId::new(),
        );
        let draft_store = DraftStore::new(dir).unwrap();
        GcAgent::new(
            GcConfig::default(),
            signal_detector,
            draft_store,
            dir.to_path_buf(),
        )
    }

    fn test_signal() -> Signal {
        Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({"test": true}),
            RemediationType::Guard,
        )
    }

    fn test_draft(artifacts: Vec<Artifact>) -> Draft {
        Draft {
            id: DraftId::new(),
            status: DraftStatus::Pending,
            signal: test_signal(),
            artifacts,
            rationale: "test".into(),
            validation: "test".into(),
            generated_at: Utc::now(),
            agent_model: "test".into(),
        }
    }

    #[test]
    fn artifact_path_under_prefix_accepts_paths_inside_prefix() {
        assert!(artifact_path_under_prefix(
            Path::new(".harness/generated/rules/new.md"),
            ".harness/generated/"
        ));
        assert!(artifact_path_under_prefix(
            Path::new(".harness/generated/rules/./new.md"),
            ".harness/generated/"
        ));
    }

    #[test]
    fn artifact_path_under_prefix_rejects_paths_outside_prefix() {
        assert!(!artifact_path_under_prefix(
            Path::new("src/main.rs"),
            ".harness/generated/"
        ));
        assert!(!artifact_path_under_prefix(
            Path::new("/etc/passwd"),
            ".harness/generated/"
        ));
        assert!(!artifact_path_under_prefix(
            Path::new(".harness/generated/../../etc/passwd"),
            ".harness/generated/"
        ));
    }

    #[test]
    fn auto_adopt_matching_off_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());
        let draft = test_draft(vec![]);
        gc.draft_store.save(&draft).unwrap();
        let adopted = gc.auto_adopt_matching(
            std::slice::from_ref(&draft.id),
            AutoAdoptPolicy::Off,
            ".harness/generated/",
        );
        assert!(adopted.is_empty());
    }

    #[test]
    fn auto_adopt_matching_rules_only_adopts_rule_drafts_under_prefix() {
        let sandbox = tempfile::tempdir().unwrap();
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let signal_detector = SignalDetector::new(
            crate::signal_detector::SignalThresholds::default(),
            ProjectId::new(),
        );
        let draft_store = DraftStore::new(sandbox.path()).unwrap();
        let gc = GcAgent::new(
            GcConfig::default(),
            signal_detector,
            draft_store,
            project_root.clone(),
        );

        // Eligible: Rule remediation + artifact under prefix.
        let eligible = Draft {
            signal: Signal::new(
                SignalType::RepeatedWarn,
                ProjectId::new(),
                serde_json::json!({}),
                RemediationType::Rule,
            ),
            ..test_draft(vec![Artifact {
                artifact_type: ArtifactType::Rule,
                target_path: PathBuf::from(".harness/generated/rules/new.md"),
                content: "rule".into(),
            }])
        };
        gc.draft_store.save(&eligible).unwrap();

        // Ineligible — remediation is Guard.
        let guard_draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: PathBuf::from(".harness/generated/guards/g.sh"),
            content: "guard".into(),
        }]);
        gc.draft_store.save(&guard_draft).unwrap();

        // Ineligible — artifact outside prefix.
        let outside = Draft {
            signal: Signal::new(
                SignalType::RepeatedWarn,
                ProjectId::new(),
                serde_json::json!({}),
                RemediationType::Rule,
            ),
            ..test_draft(vec![Artifact {
                artifact_type: ArtifactType::Rule,
                target_path: PathBuf::from("src/main.rs"),
                content: "rule".into(),
            }])
        };
        gc.draft_store.save(&outside).unwrap();

        let adopted = gc.auto_adopt_matching(
            &[
                eligible.id.clone(),
                guard_draft.id.clone(),
                outside.id.clone(),
            ],
            AutoAdoptPolicy::RulesOnly,
            ".harness/generated/",
        );
        assert_eq!(adopted, vec![eligible.id.clone()]);

        let reloaded = gc.draft_store.get(&eligible.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Adopted);
        let written =
            std::fs::read_to_string(project_root.join(".harness/generated/rules/new.md")).unwrap();
        assert_eq!(written, "rule");

        let guard_reloaded = gc.draft_store.get(&guard_draft.id).unwrap().unwrap();
        assert_eq!(guard_reloaded.status, DraftStatus::Pending);
        let outside_reloaded = gc.draft_store.get(&outside.id).unwrap().unwrap();
        assert_eq!(outside_reloaded.status, DraftStatus::Pending);
    }

    #[test]
    fn adopt_rejects_absolute_path_outside_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());

        let draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from("/etc/passwd"),
            content: "malicious".into(),
        }]);
        gc.draft_store.save(&draft).unwrap();

        let err = gc.adopt(&draft.id).unwrap_err();
        assert!(
            err.to_string().contains("outside project root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adopt_rejects_parent_dir_traversal() {
        let sandbox = tempfile::tempdir().unwrap();
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let signal_detector = SignalDetector::new(
            crate::signal_detector::SignalThresholds::default(),
            ProjectId::new(),
        );
        let draft_store = DraftStore::new(&project_root).unwrap();
        let gc = GcAgent::new(
            GcConfig::default(),
            signal_detector,
            draft_store,
            project_root.clone(),
        );

        let draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Skill,
            target_path: std::path::PathBuf::from("../../etc/shadow"),
            content: "traversal".into(),
        }]);
        gc.draft_store.save(&draft).unwrap();

        let err = gc.adopt(&draft.id).unwrap_err();
        assert!(
            err.to_string().contains("outside project root"),
            "unexpected error: {err}"
        );
        // Verify no file was written outside the project root
        let escaped = sandbox.path().join("etc/shadow");
        assert!(!escaped.exists());
    }

    #[test]
    fn adopt_accepts_valid_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());

        let draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from("subdir/test.md"),
            content: "valid content".into(),
        }]);
        gc.draft_store.save(&draft).unwrap();

        gc.adopt(&draft.id).unwrap();
        assert!(dir.path().join("subdir/test.md").exists());
    }

    // --- parse_artifacts tests ---

    fn make_signal(remediation: RemediationType) -> Signal {
        Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({}),
            remediation,
        )
    }

    #[test]
    fn parse_artifacts_falls_back_to_single_blob_when_no_structure() {
        let signal = make_signal(RemediationType::Guard);
        let output = "Some plain text output with no code blocks or diffs.";
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].content, output);
        assert!(artifacts[0].target_path.to_string_lossy().ends_with(".md"));
    }

    #[test]
    fn parse_artifacts_extracts_code_blocks_with_paths() {
        let signal = make_signal(RemediationType::Guard);
        let output = "Here are the changes:\n\
            ```rust src/main.rs\n\
            fn main() {\n    println!(\"hello\");\n}\n\
            ```\n\
            And also:\n\
            ```python tests/test_foo.py\n\
            def test_foo():\n    pass\n\
            ```\n";
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 2);

        let paths: Vec<_> = artifacts
            .iter()
            .map(|a| a.target_path.to_string_lossy().into_owned())
            .collect();
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(paths.contains(&"tests/test_foo.py".to_string()));

        let main_rs = artifacts
            .iter()
            .find(|a| a.target_path.to_string_lossy() == "src/main.rs")
            .unwrap();
        assert!(main_rs.content.contains("fn main()"));
    }

    #[test]
    fn parse_artifacts_skips_code_blocks_without_path() {
        let signal = make_signal(RemediationType::Rule);
        let output = "```rust\nfn foo() {}\n```\n";
        let artifacts = parse_artifacts(output, &signal);
        // No path in the block header → fall back to single blob
        assert_eq!(artifacts.len(), 1);
    }

    #[test]
    fn parse_artifacts_extracts_unified_diff() {
        let signal = make_signal(RemediationType::Guard);
        let output = "Apply this patch:\n\
            --- a/src/lib.rs\n\
            +++ b/src/lib.rs\n\
            @@ -1,3 +1,4 @@\n\
             fn foo() {}\n\
            +fn bar() {}\n";
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0]
            .target_path
            .to_string_lossy()
            .starts_with(".harness/drafts/"));
        assert!(artifacts[0]
            .target_path
            .to_string_lossy()
            .ends_with(".patch"));
        assert!(artifacts[0].content.contains("+++ b/src/lib.rs"));
    }

    #[test]
    fn parse_artifacts_multiple_diff_hunks() {
        let signal = make_signal(RemediationType::Hook);
        let output = "\
            --- a/a.rs\n\
            +++ b/a.rs\n\
            @@ -1 +1 @@\n\
            -old\n\
            +new\n\
            --- a/b.rs\n\
            +++ b/b.rs\n\
            @@ -1 +1 @@\n\
            -foo\n\
            +bar\n";
        let artifacts = parse_artifacts(output, &signal);
        assert_eq!(artifacts.len(), 2);
        let paths: Vec<_> = artifacts
            .iter()
            .map(|a| a.target_path.to_string_lossy().into_owned())
            .collect();
        assert!(paths.iter().all(|p| p.starts_with(".harness/drafts/")));
        assert!(paths.iter().all(|p| p.ends_with(".patch")));
    }

    #[test]
    fn parse_artifacts_code_block_takes_precedence_over_diff_same_path() {
        let signal = make_signal(RemediationType::Skill);
        let output = "\
            ```rust src/lib.rs\n\
            fn full_content() {}\n\
            ```\n\
            --- a/src/lib.rs\n\
            +++ b/src/lib.rs\n\
            @@ -1 +1 @@\n\
            +fn full_content() {}\n";
        let artifacts = parse_artifacts(output, &signal);
        // Deduplication: same path, code block wins
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].content.contains("fn full_content()"));
        // Content should be from the code block, not the diff header
        assert!(!artifacts[0].content.contains("+++ b/src/lib.rs"));
    }

    #[test]
    fn adopt_rejected_draft_errors() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());

        let mut draft = test_draft(vec![]);
        draft.status = DraftStatus::Rejected;
        gc.draft_store.save(&draft).unwrap();

        let err = gc.adopt(&draft.id).unwrap_err();
        assert!(
            err.to_string().contains("Rejected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn auto_adopt_skips_rejected() {
        let sandbox = tempfile::tempdir().unwrap();
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let signal_detector = SignalDetector::new(
            crate::signal_detector::SignalThresholds::default(),
            ProjectId::new(),
        );
        let draft_store = DraftStore::new(sandbox.path()).unwrap();
        let gc = GcAgent::new(
            GcConfig::default(),
            signal_detector,
            draft_store,
            project_root.clone(),
        );

        let mut rejected = Draft {
            signal: Signal::new(
                SignalType::RepeatedWarn,
                ProjectId::new(),
                serde_json::json!({}),
                RemediationType::Rule,
            ),
            ..test_draft(vec![Artifact {
                artifact_type: ArtifactType::Rule,
                target_path: PathBuf::from(".harness/generated/rules/rejected.md"),
                content: "should not be written".into(),
            }])
        };
        rejected.status = DraftStatus::Rejected;
        gc.draft_store.save(&rejected).unwrap();

        let adopted = gc.auto_adopt_matching(
            std::slice::from_ref(&rejected.id),
            AutoAdoptPolicy::RulesOnly,
            ".harness/generated/",
        );
        assert!(adopted.is_empty(), "rejected draft should not be adopted");

        let reloaded = gc.draft_store.get(&rejected.id).unwrap().unwrap();
        assert_eq!(reloaded.status, DraftStatus::Rejected);
        assert!(!project_root
            .join(".harness/generated/rules/rejected.md")
            .exists());
    }
}
