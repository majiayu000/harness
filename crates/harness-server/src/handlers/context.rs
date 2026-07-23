use crate::http::AppState;
use harness_context::{
    providers::{
        ContractProvider, ErrorProvider, ExecPlanProvider, GcDraftsProvider, RulesProvider,
        SkillsProvider, TaskBriefProvider,
    },
    ComposeConfig, ComposeMode, ComposeRequest, Composition, ContextComposer, ContextItem,
    Degraded, ItemClass, ItemId, Priority, TaskProfile,
};
use harness_core::run_id::RunIdentity;
use harness_protocol::context::{
    ContextPreviewDegraded, ContextPreviewItem, ContextPreviewItemClass, ContextPreviewPriority,
    ContextPreviewRequest, ContextPreviewTaskProfile,
};
use harness_protocol::methods::{RpcResponse, INTERNAL_ERROR};

pub async fn context_preview(
    state: &AppState,
    id: Option<serde_json::Value>,
    request: ContextPreviewRequest,
    supplied_items: Vec<ContextPreviewItem>,
) -> RpcResponse {
    let mut request = into_compose_request(request);
    let supplied_items = supplied_items.into_iter().map(into_context_item).collect();
    request.run_id = request.run_id.or_else(current_run_id);
    let composer = build_composer(state, ComposeMode::Preview).await;
    match composer.compose_supplied(&request, supplied_items) {
        Ok(composition) => RpcResponse::success(id, composition_response(composition)),
        Err(error) => RpcResponse::error(id, INTERNAL_ERROR, error.to_string()),
    }
}

fn into_compose_request(request: ContextPreviewRequest) -> ComposeRequest {
    ComposeRequest {
        thread_id: request.thread_id,
        run_id: request.run_id,
        project: request.project,
        task_profile: into_task_profile(request.task_profile),
        budget_hint: request.budget_hint,
    }
}

fn into_task_profile(task_profile: ContextPreviewTaskProfile) -> TaskProfile {
    TaskProfile {
        task_kind: task_profile.task_kind,
        target_paths: task_profile.target_paths,
        agent_kind: task_profile.agent_kind,
        prompt: task_profile.prompt,
        contract: task_profile.contract,
    }
}

fn into_context_item(item: ContextPreviewItem) -> ContextItem {
    ContextItem {
        id: ItemId::new(item.id.0),
        class: into_item_class(item.class),
        content: item.content,
        est_tokens: item.est_tokens,
        priority: into_priority(item.priority),
        relevance: item.relevance,
        degrade: item.degrade.into_iter().map(into_degraded).collect(),
        dedupe_key: item.dedupe_key,
        instruction_bearing: item.instruction_bearing,
    }
}

fn into_item_class(class: ContextPreviewItemClass) -> ItemClass {
    match class {
        ContextPreviewItemClass::Rule => ItemClass::Rule,
        ContextPreviewItemClass::Skill => ItemClass::Skill,
        ContextPreviewItemClass::Contract => ItemClass::Contract,
        ContextPreviewItemClass::Brief => ItemClass::Brief,
        ContextPreviewItemClass::Draft => ItemClass::Draft,
    }
}

fn into_priority(priority: ContextPreviewPriority) -> Priority {
    match priority {
        ContextPreviewPriority::P0 => Priority::P0,
        ContextPreviewPriority::P1 => Priority::P1,
        ContextPreviewPriority::P2 => Priority::P2,
    }
}

fn into_degraded(degraded: ContextPreviewDegraded) -> Degraded {
    match degraded {
        ContextPreviewDegraded::Summary(content) => Degraded::Summary(content),
        ContextPreviewDegraded::Pointer(content) => Degraded::Pointer(content),
        ContextPreviewDegraded::Summarized { text, nap } => Degraded::Summarized { text, nap },
    }
}

async fn build_composer(state: &AppState, mode: ComposeMode) -> ContextComposer {
    let mut config = ComposeConfig::from(&state.core.server.config.context);
    config.mode = mode;

    let rules = {
        let rules = state.engines.rules.read().await;
        rules.rules().to_vec()
    };
    let skills = {
        let skills = state.engines.skills.read().await;
        skills.list().to_vec()
    };
    let plans = state
        .core
        .plan_cache
        .iter()
        .map(|entry| entry.value().clone())
        .collect::<Vec<_>>();
    let composer = ContextComposer::new(config)
        .with_provider(Box::new(ContractProvider))
        .with_provider(Box::new(ExecPlanProvider::new(plans)))
        .with_provider(Box::new(RulesProvider::new(rules)))
        .with_provider(Box::new(SkillsProvider::new(skills)))
        .with_provider(Box::new(TaskBriefProvider));

    match state.engines.gc_agent.drafts() {
        Ok(drafts) => composer.with_provider(Box::new(GcDraftsProvider::new(drafts))),
        Err(error) => {
            tracing::error!(error = %error, "context gc-drafts provider snapshot failed");
            composer.with_provider(Box::new(ErrorProvider::new(
                "gc-drafts",
                format!("snapshot failed: {error}"),
            )))
        }
    }
}

fn composition_response(composition: Composition) -> serde_json::Value {
    serde_json::json!({
        "rendered": composition.rendered,
        "manifest": composition.manifest,
    })
}

fn current_run_id() -> Option<harness_core::run_id::RunId> {
    RunIdentity::from_env()
        .ok()
        .flatten()
        .map(|identity| identity.run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::compress::NapStatus;
    use harness_core::types::{ProjectId, ThreadId};
    use harness_protocol::context::ContextPreviewItemId;

    fn full_request() -> ContextPreviewRequest {
        ContextPreviewRequest {
            thread_id: ThreadId::from_str("thread-conversion"),
            run_id: Some(
                "ar-01j1qb3c9r7v5m2k8x4tznq6wf"
                    .parse()
                    .expect("valid run id"),
            ),
            project: ProjectId::from_str("project-conversion"),
            task_profile: ContextPreviewTaskProfile {
                task_kind: Some("implementation".to_string()),
                target_paths: vec!["src/lib.rs".into(), "tests/context.rs".into()],
                agent_kind: Some("codex".to_string()),
                prompt: Some("Preserve every field.".to_string()),
                contract: Some("context-preview".to_string()),
            },
            budget_hint: 2048,
        }
    }

    fn full_items() -> Vec<ContextPreviewItem> {
        vec![
            ContextPreviewItem {
                id: ContextPreviewItemId::new("rule:first"),
                class: ContextPreviewItemClass::Rule,
                content: "first content".to_string(),
                est_tokens: 11,
                priority: ContextPreviewPriority::P0,
                relevance: 1.0,
                degrade: vec![
                    ContextPreviewDegraded::Summary("first summary".to_string()),
                    ContextPreviewDegraded::Pointer("first pointer".to_string()),
                    ContextPreviewDegraded::Summarized {
                        text: "first compressed".to_string(),
                        nap: NapStatus::Verified,
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "second compressed".to_string(),
                        nap: NapStatus::SkippedSample,
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "failed compressed".to_string(),
                        nap: NapStatus::Failed { fell_back: true },
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "failed without fallback".to_string(),
                        nap: NapStatus::Failed { fell_back: false },
                    },
                ],
                dedupe_key: Some("rule-first".to_string()),
                instruction_bearing: true,
            },
            ContextPreviewItem {
                id: ContextPreviewItemId::new("skill:second"),
                class: ContextPreviewItemClass::Skill,
                content: String::new(),
                est_tokens: 0,
                priority: ContextPreviewPriority::P1,
                relevance: 0.5,
                degrade: Vec::new(),
                dedupe_key: None,
                instruction_bearing: false,
            },
            ContextPreviewItem {
                id: ContextPreviewItemId::new("contract:third"),
                class: ContextPreviewItemClass::Contract,
                content: "third content".to_string(),
                est_tokens: 33,
                priority: ContextPreviewPriority::P2,
                relevance: 0.0,
                degrade: Vec::new(),
                dedupe_key: Some("contract-third".to_string()),
                instruction_bearing: true,
            },
            ContextPreviewItem {
                id: ContextPreviewItemId::new("brief:fourth"),
                class: ContextPreviewItemClass::Brief,
                content: "fourth content".to_string(),
                est_tokens: 44,
                priority: ContextPreviewPriority::P1,
                relevance: 0.25,
                degrade: Vec::new(),
                dedupe_key: None,
                instruction_bearing: false,
            },
            ContextPreviewItem {
                id: ContextPreviewItemId::new("draft:fifth"),
                class: ContextPreviewItemClass::Draft,
                content: "fifth content".to_string(),
                est_tokens: 55,
                priority: ContextPreviewPriority::P2,
                relevance: 0.75,
                degrade: Vec::new(),
                dedupe_key: None,
                instruction_bearing: true,
            },
        ]
    }

    #[test]
    fn context_preview_conversion_preserves_every_field() {
        let request = into_compose_request(full_request());
        assert_eq!(request.thread_id.as_str(), "thread-conversion");
        assert_eq!(
            request.run_id.as_ref().map(|run_id| run_id.as_str()),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6wf")
        );
        assert_eq!(request.project.as_str(), "project-conversion");
        assert_eq!(
            request.task_profile.task_kind.as_deref(),
            Some("implementation")
        );
        assert_eq!(
            request.task_profile.target_paths,
            vec![
                std::path::PathBuf::from("src/lib.rs"),
                std::path::PathBuf::from("tests/context.rs")
            ]
        );
        assert_eq!(request.task_profile.agent_kind.as_deref(), Some("codex"));
        assert_eq!(
            request.task_profile.prompt.as_deref(),
            Some("Preserve every field.")
        );
        assert_eq!(
            request.task_profile.contract.as_deref(),
            Some("context-preview")
        );
        assert_eq!(request.budget_hint, 2048);

        let source_items = full_items();
        let source_items_json =
            serde_json::to_value(&source_items).expect("serialize source items");
        let items: Vec<_> = source_items.into_iter().map(into_context_item).collect();
        assert_eq!(
            serde_json::to_value(&items).expect("serialize converted items"),
            source_items_json
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "rule:first",
                "skill:second",
                "contract:third",
                "brief:fourth",
                "draft:fifth"
            ]
        );
        assert_eq!(
            items.iter().map(|item| item.class).collect::<Vec<_>>(),
            vec![
                ItemClass::Rule,
                ItemClass::Skill,
                ItemClass::Contract,
                ItemClass::Brief,
                ItemClass::Draft
            ]
        );
        assert_eq!(
            items.iter().map(|item| item.priority).collect::<Vec<_>>(),
            vec![
                Priority::P0,
                Priority::P1,
                Priority::P2,
                Priority::P1,
                Priority::P2
            ]
        );
        assert_eq!(items[0].content, "first content");
        assert_eq!(items[0].est_tokens, 11);
        assert_eq!(items[0].relevance, 1.0);
        assert_eq!(items[0].dedupe_key.as_deref(), Some("rule-first"));
        assert!(items[0].instruction_bearing);
        assert_eq!(
            items[0].degrade,
            vec![
                Degraded::Summary("first summary".to_string()),
                Degraded::Pointer("first pointer".to_string()),
                Degraded::Summarized {
                    text: "first compressed".to_string(),
                    nap: NapStatus::Verified
                },
                Degraded::Summarized {
                    text: "second compressed".to_string(),
                    nap: NapStatus::SkippedSample
                },
                Degraded::Summarized {
                    text: "failed compressed".to_string(),
                    nap: NapStatus::Failed { fell_back: true }
                },
                Degraded::Summarized {
                    text: "failed without fallback".to_string(),
                    nap: NapStatus::Failed { fell_back: false }
                }
            ]
        );
        assert_eq!(items[1].content, "");
        assert_eq!(items[1].est_tokens, 0);
        assert_eq!(items[1].relevance, 0.5);
        assert_eq!(items[1].dedupe_key, None);
        assert!(!items[1].instruction_bearing);
    }

    #[test]
    fn context_preview_conversion_is_deterministic_and_order_preserving() {
        let source_request = full_request();
        let source_items = full_items();
        let retained_request = source_request.clone();
        let retained_items = source_items.clone();

        let first_request = into_compose_request(source_request.clone());
        let second_request = into_compose_request(source_request.clone());
        let first_items: Vec<_> = source_items
            .clone()
            .into_iter()
            .map(into_context_item)
            .collect();
        let second_items: Vec<_> = source_items
            .clone()
            .into_iter()
            .map(into_context_item)
            .collect();

        assert_eq!(first_request, second_request);
        assert_eq!(first_items, second_items);
        assert_eq!(source_request, retained_request);
        assert_eq!(source_items, retained_items);
        assert_eq!(
            first_items[0]
                .degrade
                .iter()
                .map(|degraded| match degraded {
                    Degraded::Summary(_) => "summary",
                    Degraded::Pointer(_) => "pointer",
                    Degraded::Summarized { .. } => "summarized",
                })
                .collect::<Vec<_>>(),
            vec![
                "summary",
                "pointer",
                "summarized",
                "summarized",
                "summarized",
                "summarized"
            ]
        );
    }
}
