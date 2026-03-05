use harness_core::{
    AgentRequest, Artifact, ArtifactType, CodeAgent, Draft, DraftId, DraftStatus,
    Project, Signal, SignalType, RemediationType,
};
use crate::draft_store::DraftStore;
use crate::remediation::signal_priority;
use crate::signal_detector::SignalDetector;
use chrono::Utc;
use serde::{Deserialize, Serialize};

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
                    errors.push(format!("agent failed for signal {:?}: {e}", signal.signal_type));
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
        target_path: std::path::PathBuf::from(format!(
            ".harness/drafts/{}.md",
            signal.id
        )),
        content: output.to_string(),
    }]
}
