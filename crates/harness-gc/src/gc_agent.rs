use crate::draft_store::DraftStore;
use crate::remediation::signal_priority;
use crate::signal_detector::SignalDetector;
use anyhow::Context;
use chrono::Utc;
use harness_core::{
    AgentRequest, Artifact, ArtifactType, CodeAgent, Draft, DraftId, DraftStatus, Project,
    RemediationType, Signal, SignalType,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcConfig {
    pub max_drafts_per_run: usize,
    pub budget_per_signal_usd: f64,
    pub total_budget_usd: f64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_drafts_per_run: 5,
            budget_per_signal_usd: 0.50,
            total_budget_usd: 5.0,
        }
    }
}

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
    project_root: PathBuf,
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
    pub fn adopt(&self, draft_id: &DraftId) -> anyhow::Result<()> {
        let mut draft = self
            .draft_store
            .get(draft_id)?
            .ok_or_else(|| anyhow::anyhow!("draft not found"))?;
        let canonical_project_root = self.project_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize project root '{}'",
                self.project_root.display()
            )
        })?;

        for artifact in &draft.artifacts {
            let validated_target_path = match validate_target_path_for_write(
                &canonical_project_root,
                &artifact.target_path,
            ) {
                Ok(path) => path,
                Err(err) => {
                    tracing::error!(
                        draft_id = %draft_id,
                        target_path = %artifact.target_path.display(),
                        project_root = %canonical_project_root.display(),
                        error = %err,
                        "GC adopt rejected unsafe artifact target_path"
                    );
                    return Err(err);
                }
            };

            if let Some(parent) = validated_target_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&validated_target_path, &artifact.content)?;
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

fn validate_target_path_for_write(
    project_root: &Path,
    target_path: &Path,
) -> anyhow::Result<PathBuf> {
    let absolute_target = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        project_root.join(target_path)
    };
    let normalized_target = normalize_path(&absolute_target);
    let resolved_target =
        resolve_path_for_boundary_check(&normalized_target).with_context(|| {
            format!(
                "invalid artifact target_path '{}': failed to resolve path",
                target_path.display()
            )
        })?;

    if resolved_target == project_root || resolved_target.starts_with(project_root) {
        return Ok(normalized_target);
    }

    anyhow::bail!(
        "invalid artifact target_path '{}': resolved path '{}' is outside project root '{}'",
        target_path.display(),
        resolved_target.display(),
        project_root.display()
    );
}

fn resolve_path_for_boundary_check(path: &Path) -> anyhow::Result<PathBuf> {
    let mut existing_ancestor = path;
    while !existing_ancestor.exists() {
        existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "invalid artifact target_path '{}': no existing ancestor",
                path.display()
            )
        })?;
    }

    let canonical_ancestor = existing_ancestor.canonicalize()?;
    let remaining_suffix = path
        .strip_prefix(existing_ancestor)
        .map_err(|_| anyhow::anyhow!("failed to compute suffix for '{}'", path.display()))?;
    Ok(canonical_ancestor.join(remaining_suffix))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    normalized
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
mod tests {
    use super::*;
    use harness_core::{ProjectId, RemediationType};
    use serde_json::json;

    fn make_agent(project_root: &Path) -> anyhow::Result<GcAgent> {
        let data_dir = project_root.join(".harness");
        let draft_store = DraftStore::new(&data_dir)?;
        let signal_detector = SignalDetector::new(Default::default(), ProjectId::new());
        Ok(GcAgent::new(
            GcConfig::default(),
            signal_detector,
            draft_store,
            project_root.to_path_buf(),
        ))
    }

    fn make_draft(target_path: PathBuf) -> Draft {
        let signal = Signal::new(
            SignalType::LinterViolations,
            ProjectId::new(),
            json!({ "source": "test" }),
            RemediationType::Guard,
        );

        Draft {
            id: DraftId::new(),
            status: DraftStatus::Pending,
            signal,
            artifacts: vec![Artifact {
                artifact_type: ArtifactType::Guard,
                target_path,
                content: "guard content".to_string(),
            }],
            rationale: "test draft".to_string(),
            validation: "test validation".to_string(),
            generated_at: Utc::now(),
            agent_model: "test-model".to_string(),
        }
    }

    #[test]
    fn adopt_rejects_path_traversal_target_path() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = make_agent(&project_root)?;
        let draft = make_draft(PathBuf::from("../escaped.txt"));
        let draft_id = draft.id.clone();
        agent.draft_store().save(&draft)?;

        let err = agent
            .adopt(&draft_id)
            .expect_err("path traversal should be rejected");
        assert!(err.to_string().contains("outside project root"));
        let escaped_path = project_root
            .parent()
            .expect("project root has parent")
            .join("escaped.txt");
        assert!(!escaped_path.exists(), "escaped path must not be written");
        let persisted = agent
            .draft_store()
            .get(&draft_id)?
            .expect("draft should remain saved");
        assert_eq!(persisted.status, DraftStatus::Pending);
        Ok(())
    }

    #[test]
    fn adopt_rejects_absolute_outside_root_target_path() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let outside_path = sandbox.path().join("outside").join("escape.txt");
        let agent = make_agent(&project_root)?;
        let draft = make_draft(outside_path.clone());
        let draft_id = draft.id.clone();
        agent.draft_store().save(&draft)?;

        let err = agent
            .adopt(&draft_id)
            .expect_err("absolute outside-root path should be rejected");
        assert!(err.to_string().contains("outside project root"));
        assert!(!outside_path.exists(), "outside path must not be written");
        let persisted = agent
            .draft_store()
            .get(&draft_id)?
            .expect("draft should remain saved");
        assert_eq!(persisted.status, DraftStatus::Pending);
        Ok(())
    }

    #[test]
    fn adopt_writes_artifact_within_project_root() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let agent = make_agent(&project_root)?;
        let draft = make_draft(PathBuf::from("rules/new-guard.md"));
        let draft_id = draft.id.clone();
        agent.draft_store().save(&draft)?;

        agent.adopt(&draft_id)?;

        let written_path = project_root.join("rules/new-guard.md");
        let written = std::fs::read_to_string(&written_path)?;
        assert_eq!(written, "guard content");
        let persisted = agent
            .draft_store()
            .get(&draft_id)?
            .expect("draft should remain saved");
        assert_eq!(persisted.status, DraftStatus::Adopted);
        Ok(())
    }
}
