use crate::checkpoint::{filter_events_since, GcCheckpoint};
use crate::draft_store::DraftStore;
use crate::remediation::signal_priority;
use crate::signal_detector::SignalDetector;
use chrono::Utc;
use harness_core::{
    AgentRequest, Artifact, ArtifactType, CodeAgent, Draft, DraftId, DraftStatus, GcConfig,
    Project, RemediationType, Signal, SignalType,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    /// Path to the checkpoint file; `None` disables checkpoint-based scanning.
    checkpoint_path: Option<PathBuf>,
}

impl GcAgent {
    pub fn new(config: GcConfig, signal_detector: SignalDetector, draft_store: DraftStore) -> Self {
        Self {
            config,
            signal_detector,
            draft_store,
            checkpoint_path: None,
        }
    }

    /// Configure the checkpoint file path used for incremental scanning.
    pub fn with_checkpoint(mut self, path: PathBuf) -> Self {
        self.checkpoint_path = Some(path);
        self
    }

    /// Run a GC cycle using incremental scanning when a checkpoint is configured.
    ///
    /// If a valid checkpoint exists only events since `last_scan_at` are analysed.
    /// Falls back to a full scan when the checkpoint is missing or corrupt.
    /// On success the checkpoint is updated to the current timestamp.
    pub async fn run(
        &self,
        project: &Project,
        events: &[harness_core::Event],
        violations: &[harness_core::Violation],
        agent: &dyn CodeAgent,
    ) -> anyhow::Result<GcReport> {
        // Determine incremental cutoff from checkpoint (None → full scan).
        let since = self
            .checkpoint_path
            .as_deref()
            .and_then(GcCheckpoint::load)
            .map(|cp| cp.last_scan_at);

        let scanned_events = filter_events_since(events, since);

        // 1. Detect signals from events (including persisted `rule_check` violations)
        let mut signals = self.signal_detector.detect(scanned_events);
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

        let report = GcReport {
            signals,
            drafts_generated,
            errors,
        };

        // Update checkpoint so the next run only processes new events.
        if let Some(path) = &self.checkpoint_path {
            let cp = GcCheckpoint::new(Utc::now());
            if let Err(e) = cp.save(path) {
                tracing::warn!("failed to save gc checkpoint: {e}");
            }
        }

        Ok(report)
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

    // For now, treat the entire output as a single artifact
    vec![Artifact {
        artifact_type,
        target_path: std::path::PathBuf::from(format!(".harness/drafts/{}.md", signal.id)),
        content: output.to_string(),
    }]
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
}
