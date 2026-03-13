use crate::draft_store::DraftStore;
use crate::remediation::signal_priority;
use crate::signal_detector::SignalDetector;
use chrono::Utc;
use harness_core::{
    AgentRequest, Artifact, ArtifactType, CodeAgent, Draft, DraftId, DraftStatus, GcConfig,
    Project, RemediationType, Signal, SignalType,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcReport {
    pub signals: Vec<Signal>,
    pub drafts_generated: usize,
    pub errors: Vec<String>,
}

pub struct GcAgent {
    config: GcConfig,
    signal_detector: SignalDetector,
    draft_store: DraftStore,
}

impl GcAgent {
    pub fn new(config: GcConfig, signal_detector: SignalDetector, draft_store: DraftStore) -> Self {
        Self {
            config,
            signal_detector,
            draft_store,
        }
    }

    /// Run a full GC cycle: detect signals → generate fix drafts.
    pub async fn run(
        &self,
        project: &Project,
        events: &[harness_core::Event],
        violations: &[harness_core::Violation],
        agent: &dyn CodeAgent,
    ) -> anyhow::Result<GcReport> {
        // 1. Detect signals from events (including persisted `rule_check` violations)
        let mut signals = self.signal_detector.detect(events);
        // Back-compat: if the caller provided live violations but they haven't been
        // persisted into `events` yet, fall back to the old detector.
        if !violations.is_empty() && !events.iter().any(|e| e.hook == "rule_check") {
            signals.extend(self.signal_detector.from_violations(violations));
        }

        // 2. Prioritize
        signals.sort_by_key(|s| signal_priority(s.signal_type));

        // 3. Generate fix drafts (up to max_drafts_per_run)
        let mut drafts_generated = 0;
        let mut errors = Vec::new();

        for signal in signals.iter().take(self.config.max_drafts_per_run) {
            let prompt = build_prompt(signal, project);

            let result = agent
                .execute(AgentRequest {
                    prompt,
                    project_root: project.root.clone(),
                    allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
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
                    if let Err(e) = self.draft_store.save(&draft) {
                        errors.push(format!("failed to save draft: {e}"));
                    } else {
                        drafts_generated += 1;
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

        Ok(GcReport {
            signals,
            drafts_generated,
            errors,
        })
    }

    /// Adopt a draft: write artifacts to disk.
    ///
    /// All artifact target_paths are validated to be relative (no absolute
    /// paths or `..` traversal) before any writes occur.
    pub fn adopt(&self, draft_id: &DraftId) -> anyhow::Result<()> {
        let mut draft = self
            .draft_store
            .get(draft_id)?
            .ok_or_else(|| anyhow::anyhow!("draft not found"))?;

        // Validate all paths before writing any files
        for artifact in &draft.artifacts {
            let path = &artifact.target_path;
            if path.is_absolute() {
                anyhow::bail!(
                    "artifact target_path must be relative, got: {}",
                    path.display()
                );
            }
            for component in path.components() {
                if matches!(component, std::path::Component::ParentDir) {
                    anyhow::bail!(
                        "artifact target_path must not contain '..': {}",
                        path.display()
                    );
                }
            }
        }

        for artifact in &draft.artifacts {
            if let Some(parent) = artifact.target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&artifact.target_path, &artifact.content)?;
        }

        draft.status = DraftStatus::Adopted;
        self.draft_store.save(&draft)?;
        Ok(())
    }

    /// Reject a draft.
    pub fn reject(&self, draft_id: &DraftId, _reason: Option<&str>) -> anyhow::Result<()> {
        let mut draft = self
            .draft_store
            .get(draft_id)?
            .ok_or_else(|| anyhow::anyhow!("draft not found"))?;
        draft.status = DraftStatus::Rejected;
        self.draft_store.save(&draft)?;
        Ok(())
    }

    pub fn drafts(&self) -> anyhow::Result<Vec<Draft>> {
        self.draft_store.list()
    }

    pub fn draft_store(&self) -> &DraftStore {
        &self.draft_store
    }
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
    let diff_artifacts = extract_diff_artifacts(output, artifact_type);

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
fn extract_diff_artifacts(output: &str, artifact_type: ArtifactType) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        if lines[i].starts_with("--- ") && i + 1 < lines.len() && lines[i + 1].starts_with("+++ ") {
            let plus_line = lines[i + 1];
            let raw_path = plus_line
                .strip_prefix("+++ b/")
                .or_else(|| plus_line.strip_prefix("+++ "))
                .unwrap_or("")
                .trim();

            if !raw_path.is_empty() && raw_path != "/dev/null" && is_valid_file_path(raw_path) {
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
                artifacts.push(Artifact {
                    artifact_type,
                    target_path: std::path::PathBuf::from(raw_path),
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
    use harness_core::{
        Artifact, ArtifactType, Draft, DraftStatus, ProjectId, RemediationType, Signal, SignalType,
    };

    fn make_test_gc_agent(dir: &std::path::Path) -> GcAgent {
        let signal_detector = SignalDetector::new(
            crate::signal_detector::SignalThresholds::default(),
            ProjectId::new(),
        );
        let draft_store = DraftStore::new(dir).unwrap();
        GcAgent::new(GcConfig::default(), signal_detector, draft_store)
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
    fn adopt_rejects_absolute_target_path() {
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
            err.to_string().contains("must be relative"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adopt_rejects_parent_dir_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());

        let draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Skill,
            target_path: std::path::PathBuf::from("../../etc/shadow"),
            content: "traversal".into(),
        }]);
        gc.draft_store.save(&draft).unwrap();

        let err = gc.adopt(&draft.id).unwrap_err();
        assert!(
            err.to_string().contains("must not contain '..'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adopt_accepts_valid_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let gc = make_test_gc_agent(dir.path());

        let draft = test_draft(vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from(".harness/drafts/test.md"),
            content: "valid content".into(),
        }]);
        gc.draft_store.save(&draft).unwrap();

        gc.adopt(&draft.id).unwrap();
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
        assert_eq!(artifacts[0].target_path.to_string_lossy(), "src/lib.rs");
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
        assert!(paths.contains(&"a.rs".to_string()));
        assert!(paths.contains(&"b.rs".to_string()));
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
}
