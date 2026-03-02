use chrono::{DateTime, Utc};
use harness_core::{ExecPlanId, ExecPlanStatus};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecPlan {
    pub id: ExecPlanId,
    pub purpose: String,
    pub project_root: PathBuf,
    pub progress: Vec<Milestone>,
    pub concrete_steps: Vec<Step>,
    pub decision_log: Vec<PlanDecision>,
    pub surprises: Vec<Surprise>,
    pub validation: ValidationCriteria,
    pub status: ExecPlanStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Milestone {
    pub description: String,
    pub completed: bool,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub description: String,
    pub files: Vec<PathBuf>,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDecision {
    pub decision: String,
    pub rationale: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surprise {
    pub description: String,
    pub evidence: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCriteria {
    pub tests: Vec<String>,
    pub checks: Vec<String>,
}

impl Default for ValidationCriteria {
    fn default() -> Self {
        Self {
            tests: Vec::new(),
            checks: Vec::new(),
        }
    }
}

impl ExecPlan {
    /// Initialize from a SPEC document.
    pub fn from_spec(spec: &str, project_root: &std::path::Path) -> anyhow::Result<Self> {
        let purpose = spec
            .lines()
            .find(|l| l.starts_with('#'))
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .unwrap_or_else(|| "Untitled Plan".to_string());

        let now = Utc::now();
        Ok(Self {
            id: ExecPlanId::new(),
            purpose,
            project_root: project_root.to_path_buf(),
            progress: Vec::new(),
            concrete_steps: Vec::new(),
            decision_log: Vec::new(),
            surprises: Vec::new(),
            validation: ValidationCriteria::default(),
            status: ExecPlanStatus::Draft,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn activate(&mut self) {
        self.status = ExecPlanStatus::Active;
        self.updated_at = Utc::now();
    }

    pub fn update_milestone(&mut self, idx: usize, completed: bool) {
        if let Some(m) = self.progress.get_mut(idx) {
            m.completed = completed;
            m.completed_at = if completed { Some(Utc::now()) } else { None };
            self.updated_at = Utc::now();
        }
    }

    pub fn add_milestone(&mut self, description: String) {
        self.progress.push(Milestone {
            description,
            completed: false,
            completed_at: None,
        });
        self.updated_at = Utc::now();
    }

    pub fn add_step(&mut self, description: String, files: Vec<PathBuf>) {
        self.concrete_steps.push(Step {
            description,
            files,
            completed: false,
        });
        self.updated_at = Utc::now();
    }

    pub fn log_decision(&mut self, decision: &str, rationale: &str) {
        self.decision_log.push(PlanDecision {
            decision: decision.to_string(),
            rationale: rationale.to_string(),
            timestamp: Utc::now(),
        });
        self.updated_at = Utc::now();
    }

    pub fn log_surprise(&mut self, description: &str, evidence: &str) {
        self.surprises.push(Surprise {
            description: description.to_string(),
            evidence: evidence.to_string(),
            timestamp: Utc::now(),
        });
        self.updated_at = Utc::now();
    }

    pub fn complete(&mut self) {
        self.status = ExecPlanStatus::Completed;
        self.updated_at = Utc::now();
    }

    pub fn abandon(&mut self) {
        self.status = ExecPlanStatus::Abandoned;
        self.updated_at = Utc::now();
    }

    /// Serialize to Markdown (living document format).
    pub fn to_markdown(&self) -> String {
        crate::markdown::to_markdown(self)
    }

    /// Deserialize from Markdown (cross-session recovery).
    pub fn from_markdown(content: &str) -> anyhow::Result<Self> {
        crate::markdown::from_markdown(content)
    }
}
