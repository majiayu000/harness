use super::*;
use tokio::sync::RwLock;

fn many_skill_store() -> harness_skills::store::SkillStore {
    let mut store = harness_skills::store::SkillStore::new();
    for i in (0..25).rev() {
        let trigger = if i == 24 {
            "special cap trigger"
        } else {
            "never-match"
        };
        store.create(
            format!("skill-{i:02}"),
            format!(
                "# Skill {i}\n<!-- trigger-patterns: {trigger} -->\nFull content for skill {i}."
            ),
        );
    }
    store
}

#[tokio::test]
async fn inject_skills_caps_available_listing_but_keeps_matched_content() {
    let skills = RwLock::new(many_skill_store());

    let result = inject_skills_into_prompt(&skills, "please use special cap trigger").await;

    assert!(result.contains("## Available Skills"));
    assert!(result.contains("**skill-00**"));
    assert!(result.contains("**skill-19**"));
    assert!(!result.contains("**skill-20**"));
    assert!(result.contains("5 additional skills omitted"));
    assert!(result.contains("## Relevant Skills"));
    assert!(result.contains("### skill-24"));
    assert!(result.contains("Full content for skill 24."));

    let guard = skills.read().await;
    let matched = guard
        .list()
        .iter()
        .find(|skill| skill.name == "skill-24")
        .unwrap();
    assert_eq!(matched.usage_count, 1);
}
