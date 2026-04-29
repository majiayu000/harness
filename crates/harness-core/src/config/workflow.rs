use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueWorkflowPolicy {
    #[serde(default = "default_force_execute_label")]
    pub force_execute_label: String,
    #[serde(default = "default_true")]
    pub auto_replan_on_plan_issue: bool,
    /// When true, the review loop pauses at `ready_to_merge` and requires a
    /// human to call `POST /tasks/:id/merge` before the workflow advances to
    /// `done`.  Defaults to `false` to preserve the legacy auto-merge flow.
    #[serde(default)]
    pub require_human_gate_before_merge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrFeedbackPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_feedback_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    #[serde(default = "default_feedback_claim_stale_after_secs")]
    pub claim_stale_after_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowConfig {
    #[serde(default)]
    pub issue_workflow: IssueWorkflowPolicy,
    #[serde(default)]
    pub pr_feedback: PrFeedbackPolicy,
    #[serde(default)]
    pub storage: WorkflowStoragePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStoragePolicy {
    #[serde(default = "default_workflow_schema_namespace")]
    pub schema_namespace: String,
}

impl Default for IssueWorkflowPolicy {
    fn default() -> Self {
        Self {
            force_execute_label: default_force_execute_label(),
            auto_replan_on_plan_issue: default_true(),
            require_human_gate_before_merge: false,
        }
    }
}

impl Default for PrFeedbackPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            sweep_interval_secs: default_feedback_sweep_interval_secs(),
            claim_stale_after_secs: default_feedback_claim_stale_after_secs(),
        }
    }
}

impl Default for WorkflowStoragePolicy {
    fn default() -> Self {
        Self {
            schema_namespace: default_workflow_schema_namespace(),
        }
    }
}

fn default_force_execute_label() -> String {
    "force-execute".to_string()
}

fn default_feedback_sweep_interval_secs() -> u64 {
    60
}

fn default_feedback_claim_stale_after_secs() -> u64 {
    300
}

fn default_workflow_schema_namespace() -> String {
    "workflow".to_string()
}

fn default_true() -> bool {
    true
}

/// Load workflow policy from `{project_root}/WORKFLOW.md`.
///
/// Only the YAML front matter is parsed. Missing files or missing front matter
/// fall back to defaults.
pub fn load_workflow_config(project_root: &Path) -> anyhow::Result<WorkflowConfig> {
    let path = project_root.join("WORKFLOW.md");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorkflowConfig::default()),
        Err(e) => return Err(e.into()),
    };

    let Some(front_matter) = extract_front_matter(&contents) else {
        return Ok(WorkflowConfig::default());
    };
    if front_matter.trim().is_empty() {
        return Ok(WorkflowConfig::default());
    }

    serde_yaml::from_str(front_matter).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse workflow front matter at {}: {e}",
            path.display()
        )
    })
}

fn extract_front_matter(contents: &str) -> Option<&str> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_workflow_config_defaults_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg = load_workflow_config(dir.path())?;
        assert_eq!(cfg.issue_workflow.force_execute_label, "force-execute");
        assert!(cfg.pr_feedback.enabled);
        assert_eq!(cfg.pr_feedback.sweep_interval_secs, 60);
        assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 300);
        assert!(cfg.issue_workflow.auto_replan_on_plan_issue);
        assert_eq!(cfg.storage.schema_namespace, "workflow");
        assert!(!cfg.issue_workflow.require_human_gate_before_merge);
        Ok(())
    }

    #[test]
    fn load_workflow_config_reads_front_matter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            r#"---
issue_workflow:
  force_execute_label: do-not-second-guess
  auto_replan_on_plan_issue: false
  require_human_gate_before_merge: true
pr_feedback:
  enabled: false
  sweep_interval_secs: 15
  claim_stale_after_secs: 45
storage:
  schema_namespace: orchestration
---

Body
"#,
        )?;

        let cfg = load_workflow_config(dir.path())?;
        assert_eq!(
            cfg.issue_workflow.force_execute_label,
            "do-not-second-guess"
        );
        assert!(!cfg.issue_workflow.auto_replan_on_plan_issue);
        assert!(cfg.issue_workflow.require_human_gate_before_merge);
        assert!(!cfg.pr_feedback.enabled);
        assert_eq!(cfg.pr_feedback.sweep_interval_secs, 15);
        assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 45);
        assert_eq!(cfg.storage.schema_namespace, "orchestration");
        Ok(())
    }
}
