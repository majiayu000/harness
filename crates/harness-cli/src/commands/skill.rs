use super::SkillCommand;
use harness_core::config::HarnessConfig;

pub fn run(cmd: SkillCommand, config: &HarnessConfig) -> anyhow::Result<()> {
    match cmd {
        SkillCommand::List { query } => {
            let store = configured_skill_store(config)?;
            let skills = if let Some(q) = query {
                store.search(&q).into_iter().cloned().collect::<Vec<_>>()
            } else {
                store.list().to_vec()
            };
            for s in &skills {
                println!("{} [{}]: {}", s.name, s.id, s.description);
            }
            if skills.is_empty() {
                println!("No skills found");
            }
        }
        SkillCommand::Create { name, file } => {
            let content = std::fs::read_to_string(&file)?;
            let mut store = configured_skill_store(config)?;
            store.create(name.clone(), content);
            println!("Created skill: {name}");
        }
        SkillCommand::Delete { skill_id } => {
            let mut store = configured_skill_store(config)?;
            let deleted = if let Some(skill) = store.get_by_name(&skill_id).cloned() {
                store.delete(&skill.id)
            } else {
                store.delete(&harness_core::types::SkillId::from_str(&skill_id))
            };
            println!("Deleted skill: {deleted}");
        }
    }
    Ok(())
}

fn configured_skill_store(
    config: &HarnessConfig,
) -> anyhow::Result<harness_skills::store::SkillStore> {
    let project_root = std::env::current_dir()?;
    let mut store = harness_skills::store::SkillStore::new()
        .with_persist_dir(config.server.data_dir.join("skills"))
        .with_discovery(&project_root);
    store.load_builtin();
    store.discover()?;
    Ok(store)
}
