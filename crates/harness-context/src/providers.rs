use crate::{
    ComposeRequest, ContextItem, ContextProvider, Degraded, ItemClass, ItemId, Priority,
    ProviderError, ProviderId,
};
use harness_core::types::DraftStatus;
use harness_rules::engine::Rule;
use harness_skills::store::Skill;

pub struct RulesProvider {
    rules: Vec<Rule>,
}

impl RulesProvider {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }
}

impl ContextProvider for RulesProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("rules")
    }

    fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        let mut rules = self.rules.clone();
        rules.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(rules
            .into_iter()
            .map(|rule| {
                let content = format!("{}: {}\n\n{}", rule.id, rule.title, rule.description);
                ContextItem {
                    id: ItemId::new(format!("rule:{}", rule.id)),
                    class: ItemClass::Rule,
                    est_tokens: 0,
                    priority: Priority::P1,
                    relevance: 0.8,
                    degrade: vec![
                        Degraded::Summary(format!("{}: {}", rule.id, rule.title)),
                        Degraded::Pointer(format!("See rule {}", rule.id)),
                    ],
                    dedupe_key: Some(format!("rule:{}", rule.id)),
                    instruction_bearing: true,
                    content,
                }
            })
            .collect())
    }
}

pub struct SkillsProvider {
    skills: Vec<Skill>,
}

impl SkillsProvider {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }
}

impl ContextProvider for SkillsProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("skills")
    }

    fn propose(&self, req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        let prompt = req
            .task_profile
            .prompt
            .as_deref()
            .unwrap_or_default()
            .to_lowercase();
        let mut skills = self
            .skills
            .iter()
            .filter(|skill| {
                skill.trigger_patterns.is_empty()
                    || skill
                        .trigger_patterns
                        .iter()
                        .any(|pattern| prompt.contains(&pattern.to_lowercase()))
            })
            .cloned()
            .collect::<Vec<_>>();
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(skills
            .into_iter()
            .map(|skill| ContextItem {
                id: ItemId::new(format!("skill:{}", skill.name)),
                class: ItemClass::Skill,
                content: skill.content,
                est_tokens: 0,
                priority: Priority::P1,
                relevance: if skill.trigger_patterns.is_empty() {
                    0.4
                } else {
                    0.75
                },
                degrade: vec![
                    Degraded::Summary(skill.description.clone()),
                    Degraded::Pointer(format!("Use skill {}", skill.name)),
                ],
                dedupe_key: Some(format!("skill:{}", skill.name)),
                instruction_bearing: true,
            })
            .collect())
    }
}

pub struct ContractProvider;

impl ContextProvider for ContractProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("contract")
    }

    fn propose(&self, req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        let Some(contract) = req.task_profile.contract.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(vec![ContextItem {
            id: ItemId::new("contract:task"),
            class: ItemClass::Contract,
            content: contract.clone(),
            est_tokens: 0,
            priority: Priority::P0,
            relevance: 1.0,
            degrade: vec![Degraded::Pointer("See task contract".to_string())],
            dedupe_key: Some("contract:task".to_string()),
            instruction_bearing: true,
        }])
    }
}

pub struct TaskBriefProvider;

impl ContextProvider for TaskBriefProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("brief")
    }

    fn propose(&self, req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        let Some(prompt) = req.task_profile.prompt.as_ref() else {
            return Ok(Vec::new());
        };
        Ok(vec![ContextItem {
            id: ItemId::new("brief:task"),
            class: ItemClass::Brief,
            content: prompt.clone(),
            est_tokens: 0,
            priority: Priority::P0,
            relevance: 1.0,
            degrade: vec![Degraded::Pointer("See task brief".to_string())],
            dedupe_key: Some("brief:task".to_string()),
            instruction_bearing: false,
        }])
    }
}

pub struct GcDraftsProvider {
    drafts: Vec<harness_core::types::Draft>,
}

impl GcDraftsProvider {
    pub fn new(drafts: Vec<harness_core::types::Draft>) -> Self {
        Self { drafts }
    }
}

impl ContextProvider for GcDraftsProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("gc-drafts")
    }

    fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        let mut drafts = self
            .drafts
            .iter()
            .filter(|draft| draft.status == DraftStatus::Pending)
            .cloned()
            .collect::<Vec<_>>();
        drafts.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(drafts
            .into_iter()
            .map(|draft| ContextItem {
                id: ItemId::new(format!("draft:{}", draft.id)),
                class: ItemClass::Draft,
                content: format!("{}\n\n{}", draft.rationale, draft.validation),
                est_tokens: 0,
                priority: Priority::P2,
                relevance: 0.3,
                degrade: vec![
                    Degraded::Summary(draft.rationale.clone()),
                    Degraded::Pointer(format!("See GC draft {}", draft.id)),
                ],
                dedupe_key: Some(format!("draft:{}", draft.id)),
                instruction_bearing: false,
            })
            .collect())
    }
}

pub struct ErrorProvider {
    id: ProviderId,
    message: String,
}

impl ErrorProvider {
    pub fn new(id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: ProviderId::new(id),
            message: message.into(),
        }
    }
}

impl ContextProvider for ErrorProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        Err(ProviderError {
            provider_id: self.id.clone(),
            message: self.message.clone(),
        })
    }
}

#[derive(Clone)]
pub struct StaticProvider {
    id: ProviderId,
    items: Vec<ContextItem>,
}

impl StaticProvider {
    pub fn new(id: impl Into<String>, items: Vec<ContextItem>) -> Self {
        Self {
            id: ProviderId::new(id),
            items,
        }
    }
}

impl ContextProvider for StaticProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn propose(&self, _req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError> {
        Ok(self.items.clone())
    }
}
