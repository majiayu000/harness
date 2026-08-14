use super::PlanCommand;

pub fn run(cmd: PlanCommand) -> anyhow::Result<()> {
    match cmd {
        PlanCommand::Init { spec } => {
            let content = std::fs::read_to_string(&spec)?;
            let project_root = std::env::current_dir()?;
            let plan = harness_exec::plan::ExecPlan::from_spec(&content, &project_root)?;
            let md = plan.to_markdown();
            let out_path = format!("exec-plan-{}.md", plan.id);
            std::fs::write(&out_path, &md)?;
            println!("Created ExecPlan: {out_path}");
        }
        PlanCommand::Status { plan } => {
            if std::path::Path::new(&plan).exists() {
                let content = std::fs::read_to_string(&plan)?;
                let p = harness_exec::plan::ExecPlan::from_markdown(&content)?;
                println!("Plan: {}", p.purpose);
                println!("Status: {:?}", p.status);
                let done = p.progress.iter().filter(|m| m.completed).count();
                println!("Progress: {}/{}", done, p.progress.len());
            } else {
                println!("Plan file not found: {plan}");
            }
        }
    }
    Ok(())
}
