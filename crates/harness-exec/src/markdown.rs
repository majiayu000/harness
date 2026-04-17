use crate::plan::{ExecPlan, Milestone, Step, ValidationCriteria};
use chrono::Utc;
use harness_core::{types::ExecPlanId, types::ExecPlanStatus};
use std::path::PathBuf;

pub fn to_markdown(plan: &ExecPlan) -> String {
    let mut out = String::new();

    out.push_str(&format!("# ExecPlan: {}\n\n", plan.purpose));
    out.push_str(&format!("- **ID**: {}\n", plan.id));
    out.push_str(&format!("- **Status**: {:?}\n", plan.status));
    out.push_str(&format!("- **Project**: {}\n", plan.project_root.display()));
    out.push_str(&format!("- **Created**: {}\n", plan.created_at));
    out.push_str(&format!("- **Updated**: {}\n\n", plan.updated_at));

    // Progress
    if !plan.progress.is_empty() {
        out.push_str("## Progress\n\n");
        for m in &plan.progress {
            let check = if m.completed { "x" } else { " " };
            out.push_str(&format!("- [{}] {}\n", check, m.description));
        }
        out.push('\n');
    }

    // Steps
    if !plan.concrete_steps.is_empty() {
        out.push_str("## Concrete Steps\n\n");
        for (i, step) in plan.concrete_steps.iter().enumerate() {
            let check = if step.completed { "x" } else { " " };
            out.push_str(&format!("{}. [{}] {}\n", i + 1, check, step.description));
            for f in &step.files {
                out.push_str(&format!("   - `{}`\n", f.display()));
            }
        }
        out.push('\n');
    }

    // Decision log
    if !plan.decision_log.is_empty() {
        out.push_str("## Decision Log\n\n");
        for d in &plan.decision_log {
            out.push_str(&format!(
                "### {} ({})\n\n",
                d.decision,
                d.timestamp.format("%Y-%m-%d %H:%M")
            ));
            out.push_str(&format!("{}\n\n", d.rationale));
        }
    }

    // Surprises
    if !plan.surprises.is_empty() {
        out.push_str("## Surprises\n\n");
        for s in &plan.surprises {
            out.push_str(&format!("- **{}**: {}\n", s.description, s.evidence));
        }
        out.push('\n');
    }

    // Validation
    if !plan.validation.tests.is_empty() || !plan.validation.checks.is_empty() {
        out.push_str("## Validation\n\n");
        for t in &plan.validation.tests {
            out.push_str(&format!("- [ ] Test: {t}\n"));
        }
        for c in &plan.validation.checks {
            out.push_str(&format!("- [ ] Check: {c}\n"));
        }
    }

    out
}

pub fn from_markdown(content: &str) -> anyhow::Result<ExecPlan> {
    let purpose = content
        .lines()
        .find(|l| l.starts_with("# ExecPlan:"))
        .map(|l| l.trim_start_matches("# ExecPlan:").trim().to_string())
        .unwrap_or_else(|| "Recovered Plan".to_string());

    let mut plan = ExecPlan {
        id: ExecPlanId::new(),
        purpose,
        project_root: PathBuf::from("."),
        progress: Vec::new(),
        concrete_steps: Vec::new(),
        decision_log: Vec::new(),
        surprises: Vec::new(),
        validation: ValidationCriteria::default(),
        status: ExecPlanStatus::Active,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let mut section = "";
    let mut step_idx: usize = 0;

    for line in content.lines() {
        if line.starts_with("## Progress") {
            section = "progress";
        } else if line.starts_with("## Concrete Steps") {
            section = "steps";
        } else if line.starts_with("## Decision Log") {
            section = "decisions";
        } else if line.starts_with("## Surprises") {
            section = "surprises";
        } else if line.starts_with("## Validation") {
            section = "validation";
        } else if line.starts_with("- **ID**:") {
            let id = line.trim_start_matches("- **ID**:").trim();
            plan.id = ExecPlanId::from_str(id);
        } else if line.starts_with("- **Project**:") {
            let p = line.trim_start_matches("- **Project**:").trim();
            plan.project_root = PathBuf::from(p);
        } else {
            match section {
                "progress" if line.starts_with("- [") => {
                    let completed = line.contains("[x]");
                    let desc = line
                        .trim_start_matches("- [x] ")
                        .trim_start_matches("- [ ] ")
                        .to_string();
                    if !desc.is_empty() {
                        plan.progress.push(Milestone {
                            description: desc,
                            completed,
                            completed_at: None,
                        });
                    }
                }
                "steps" => {
                    if line.contains("[x]") || line.contains("[ ]") {
                        let completed = line.contains("[x]");
                        let desc = line
                            .trim_start_matches(|c: char| {
                                c.is_ascii_digit() || c == '.' || c == ' '
                            })
                            .trim_start_matches("[x] ")
                            .trim_start_matches("[ ] ")
                            .to_string();
                        if !desc.is_empty() {
                            plan.concrete_steps.push(Step {
                                description: desc,
                                files: Vec::new(),
                                completed,
                            });
                            step_idx = plan.concrete_steps.len() - 1;
                        }
                    } else if line.trim_start().starts_with("- `") {
                        let path = line.trim().trim_start_matches("- `").trim_end_matches('`');
                        if let Some(step) = plan.concrete_steps.get_mut(step_idx) {
                            step.files.push(PathBuf::from(path));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(plan)
}
