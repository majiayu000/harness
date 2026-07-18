use super::*;
use harness_observe::event_store::EventStore;
use tokio::sync::RwLock;

fn make_skill_store_with_two_matching() -> harness_skills::store::SkillStore {
    let mut store = harness_skills::store::SkillStore::new();
    store.create(
        "review".to_string(),
        "# Review\n<!-- trigger-patterns: code review -->\nReview code carefully.".to_string(),
    );
    store.create(
        "build-fix".to_string(),
        "# BuildFix\n<!-- trigger-patterns: code review -->\nFix build issues.".to_string(),
    );
    store
}

#[tokio::test]
async fn inject_skills_multiple_matches_all_usage_counts_incremented() {
    let skills = RwLock::new(make_skill_store_with_two_matching());
    let events = EventStore::new_noop_for_tests();
    augment_prompt_with_skills(
        &skills,
        &events,
        &TaskId::new(),
        "please do a code review".to_string(),
    )
    .await;
    let guard = skills.read().await;
    for skill in guard.list() {
        assert_eq!(
            skill.usage_count, 1,
            "skill '{}' must have usage_count=1 after matching",
            skill.name
        );
    }
}

#[tokio::test]
async fn inject_skills_retired_skill_not_included() {
    // Drive a skill into Retired state via apply_governance_outcome.
    // Retired requires: scored_samples >= 30 AND quality_score < 0.30.
    // Starting from quality_score=0.5, 4 calls with fail=100 yields ~0.293 < 0.30.
    use harness_skills::store::SkillGovernanceInput;
    let mut store = harness_skills::store::SkillStore::new();
    store.create(
        "retired-review".to_string(),
        "# RetiredReview\n<!-- trigger-patterns: code review -->\nRetired review content."
            .to_string(),
    );
    let id = store.list()[0].id.clone();
    for _ in 0..4 {
        store.apply_governance_outcome(
            &id,
            SkillGovernanceInput {
                success: 0,
                fail: 100,
                unknown: 0,
            },
        );
    }
    let skills = RwLock::new(store);
    let events = EventStore::new_noop_for_tests();
    let result = augment_prompt_with_skills(
        &skills,
        &events,
        &TaskId::new(),
        "please do a code review".to_string(),
    )
    .await;
    assert!(
        !result.contains("Retired review content."),
        "retired skill content must not appear in prompt"
    );
    let guard = skills.read().await;
    assert_eq!(
        guard.list()[0].usage_count,
        0,
        "usage must not be recorded for retired skill"
    );
}

#[tokio::test]
async fn inject_skills_quarantine_injection_is_deterministic() {
    // Drive a skill into Quarantine, then verify same prompt always produces
    // the same result (canary bucket is deterministic hash, not random).
    use harness_skills::store::SkillGovernanceInput;
    let mut store = harness_skills::store::SkillStore::new();
    store.create(
        "quarantine-review".to_string(),
        "# QuarantineReview\n<!-- trigger-patterns: code review -->\nQuarantine content."
            .to_string(),
    );
    let id = store.list()[0].id.clone();
    // One call with fail=15: quality=0.43 < 0.45, samples=15 >= 10 → Quarantine.
    store.apply_governance_outcome(
        &id,
        SkillGovernanceInput {
            success: 0,
            fail: 15,
            unknown: 0,
        },
    );
    let skills = RwLock::new(store);
    let events = EventStore::new_noop_for_tests();
    let prompt = "please do a code review".to_string();
    let result1 =
        augment_prompt_with_skills(&skills, &events, &TaskId::new(), prompt.clone()).await;
    let result2 =
        augment_prompt_with_skills(&skills, &events, &TaskId::new(), prompt.clone()).await;
    assert_eq!(
        result1, result2,
        "injection result must be deterministic for the same prompt"
    );
}

// ── truncate_validation_error ─────────────────────────────────────────────────

#[test]
fn truncate_short_error_passes_through() {
    assert_eq!(truncate_validation_error("short", 100), "short");
}

#[test]
fn truncate_long_error_includes_summary() {
    let input = "x".repeat(200);
    let result = truncate_validation_error(&input, 50);
    assert!(result.starts_with(&"x".repeat(50)));
    assert!(result.contains("(output truncated, 200 chars total)"));
}

// ── process_stream_item: ApprovalRequest ─────────────────────────────────────

#[tokio::test]
async fn process_stream_item_approval_request_appends_item_and_emits_notification() {
    use crate::server::HarnessServer;
    use crate::thread_manager::ThreadManager;
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::StreamItem;
    use harness_core::config::HarnessConfig;
    use harness_core::types::AgentId;
    use harness_protocol::notifications::RpcNotification;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new(""),
    ));
    let thread_id = server.thread_manager.start_thread(PathBuf::from("/tmp"));
    let turn_id = server
        .thread_manager
        .start_turn(&thread_id, "task".to_string(), AgentId::new())
        .unwrap();

    let (notification_tx, mut notification_rx) = broadcast::channel::<RpcNotification>(16);

    process_stream_item(
        &server,
        &None,
        &notification_tx,
        &thread_id,
        &turn_id,
        StreamItem::ApprovalRequest {
            id: "req-42".to_string(),
            command: "rm -rf /tmp/test".to_string(),
        },
    )
    .await;

    // Check item was appended.
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .expect("turn must exist");
    let has_approval = turn.items.iter().any(|item| {
        matches!(
            item,
            harness_core::types::Item::ApprovalRequest {
                action,
                approved: None,
                ..
            } if action == "rm -rf /tmp/test"
        )
    });
    assert!(
        has_approval,
        "ApprovalRequest item must be appended to turn"
    );

    // Check notification was emitted.
    let notif = notification_rx
        .try_recv()
        .expect("notification must be emitted");
    match notif.notification {
        harness_protocol::notifications::Notification::ApprovalRequest {
            turn_id: notif_turn_id,
            request_id,
            command,
        } => {
            assert_eq!(notif_turn_id, turn_id);
            assert_eq!(request_id, "req-42");
            assert_eq!(command, "rm -rf /tmp/test");
        }
        other => panic!("expected ApprovalRequest notification, got {other:?}"),
    }
}

#[tokio::test]
async fn process_stream_item_warning_emits_notification() {
    use crate::server::HarnessServer;
    use crate::thread_manager::ThreadManager;
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::StreamItem;
    use harness_core::config::HarnessConfig;
    use harness_core::types::AgentId;
    use harness_protocol::notifications::RpcNotification;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new(""),
    ));
    let thread_id = server.thread_manager.start_thread(PathBuf::from("/tmp"));
    let turn_id = server
        .thread_manager
        .start_turn(&thread_id, "task".to_string(), AgentId::new())
        .unwrap();
    let (notification_tx, mut notification_rx) = broadcast::channel::<RpcNotification>(16);

    process_stream_item(
        &server,
        &None,
        &notification_tx,
        &thread_id,
        &turn_id,
        StreamItem::Warning {
            message: "careful".to_string(),
        },
    )
    .await;

    let notif = notification_rx
        .try_recv()
        .expect("warning notification must be emitted");
    match notif.notification {
        harness_protocol::notifications::Notification::Warning {
            turn_id: notif_turn_id,
            message,
        } => {
            assert_eq!(notif_turn_id, turn_id);
            assert_eq!(message, "careful");
        }
        other => panic!("expected Warning notification, got {other:?}"),
    }
}

#[tokio::test]
async fn process_stream_item_tool_output_delta_emits_notification() {
    use crate::server::HarnessServer;
    use crate::thread_manager::ThreadManager;
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::StreamItem;
    use harness_core::config::HarnessConfig;
    use harness_core::types::AgentId;
    use harness_protocol::notifications::RpcNotification;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new(""),
    ));
    let thread_id = server.thread_manager.start_thread(PathBuf::from("/tmp"));
    let turn_id = server
        .thread_manager
        .start_turn(&thread_id, "task".to_string(), AgentId::new())
        .unwrap();
    let (notification_tx, mut notification_rx) = broadcast::channel::<RpcNotification>(16);

    process_stream_item(
        &server,
        &None,
        &notification_tx,
        &thread_id,
        &turn_id,
        StreamItem::ToolOutputDelta {
            item_id: "item-1".to_string(),
            text: "cargo check\n".to_string(),
        },
    )
    .await;

    let notif = notification_rx
        .try_recv()
        .expect("tool output notification must be emitted");
    match notif.notification {
        harness_protocol::notifications::Notification::ToolOutputDelta {
            turn_id: notif_turn_id,
            item_id,
            text,
        } => {
            assert_eq!(notif_turn_id, turn_id);
            assert_eq!(item_id, "item-1");
            assert_eq!(text, "cargo check\n");
        }
        other => panic!("expected ToolOutputDelta notification, got {other:?}"),
    }
}
