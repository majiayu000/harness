use super::decision::{decide_review_loop_action, update_silence_rounds};
use super::runtime_feedback::resolve_runtime_feedback_issue_number;
use super::signals::{
    bot_fallback_chain, classify_bot, classify_pr_check_rollup_state, record_bot_comment_signal,
    review_bot_key_for_author, BotClassification, BotSignals, PrCheckRollupState,
    PullRequestSignals, ReviewBotDescriptor, ReviewBotKey, CODEX_REVIEWER_NAME,
    CODEX_REVIEW_COMMAND,
};
use super::wait_budget::ReviewWaitBudget;
use crate::task_runner::{TaskId, TaskState, TaskStatus, TaskStore};
use chrono::Utc;
use std::collections::HashMap;
use tokio::time::{Duration, Instant};
fn descriptor(key: ReviewBotKey) -> ReviewBotDescriptor {
    match key {
        ReviewBotKey::Gemini => ReviewBotDescriptor {
            key,
            reviewer_name: "gemini-code-assist[bot]".to_string(),
            review_command: "/gemini review".to_string(),
        },
        ReviewBotKey::Codex => ReviewBotDescriptor {
            key,
            reviewer_name: CODEX_REVIEWER_NAME.to_string(),
            review_command: CODEX_REVIEW_COMMAND.to_string(),
        },
    }
}

fn bot_signals() -> BotSignals {
    BotSignals::default()
}

fn review_config() -> harness_core::config::agents::AgentReviewConfig {
    harness_core::config::agents::AgentReviewConfig::default()
}

fn review_config_with_chain(chain: &[&str]) -> harness_core::config::agents::AgentReviewConfig {
    let mut config = review_config();
    config.fallback_chain = chain.iter().map(|entry| (*entry).to_string()).collect();
    config
}

#[test]
fn classifies_pr_check_rollup_states() {
    assert_eq!(
        classify_pr_check_rollup_state(Some("SUCCESS")),
        PrCheckRollupState::Success
    );
    assert_eq!(
        classify_pr_check_rollup_state(Some("FAILURE")),
        PrCheckRollupState::Failed("FAILURE".to_string())
    );
    assert_eq!(
        classify_pr_check_rollup_state(Some("PENDING")),
        PrCheckRollupState::Pending("PENDING".to_string())
    );
    assert_eq!(
        classify_pr_check_rollup_state(None),
        PrCheckRollupState::Pending("no statusCheckRollup state yet".to_string())
    );
}

#[tokio::test]
async fn review_wait_budget_zero_disables_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let budget = ReviewWaitBudget::new(Instant::now() - Duration::from_secs(3600), 0);

    let exceeded = budget.fail_if_exceeded(store.as_ref(), &task_id, 1).await?;

    assert!(!exceeded);
    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_ne!(state.status, TaskStatus::Failed);
    Ok(())
}

#[test]
fn review_wait_budget_caps_sleep_to_remaining_budget() {
    let budget = ReviewWaitBudget::new(Instant::now() - Duration::from_secs(50), 60);

    assert_eq!(budget.sleep_duration(300), Duration::from_secs(10));
}

#[test]
fn review_wait_budget_zero_leaves_sleep_uncapped() {
    let budget = ReviewWaitBudget::new(Instant::now() - Duration::from_secs(3600), 0);

    assert_eq!(budget.sleep_duration(300), Duration::from_secs(300));
}

async fn open_issue_workflow_test_store(
) -> anyhow::Result<Option<harness_workflow::issue_lifecycle::IssueWorkflowStore>> {
    if std::env::var("DATABASE_URL").is_err() {
        return Ok(None);
    }
    let dir = tempfile::tempdir()?;
    match harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
        &dir.path().join("issue_workflows.db"),
    )
    .await
    {
        Ok(store) => Ok(Some(store)),
        Err(error) => {
            tracing::warn!("issue workflow store test skipped: {error}");
            Ok(None)
        }
    }
}

#[tokio::test]
async fn runtime_feedback_issue_resolution_keeps_explicit_issue() {
    let issue_number = resolve_runtime_feedback_issue_number(
        None,
        std::path::Path::new("/tmp/project"),
        Some("owner/repo"),
        Some(42),
        99,
    )
    .await;

    assert_eq!(issue_number, Some(42));
}

#[tokio::test]
async fn runtime_feedback_issue_resolution_looks_up_pr_only_task() -> anyhow::Result<()> {
    let Some(store) = open_issue_workflow_test_store().await? else {
        return Ok(());
    };
    let project_root = tempfile::tempdir()?;
    let project_id = project_root.path().to_string_lossy().into_owned();
    store
        .record_issue_scheduled(
            &project_id,
            Some("owner/repo"),
            123,
            "task-issue",
            &[],
            false,
        )
        .await?;
    store
        .record_pr_detected(
            &project_id,
            Some("owner/repo"),
            123,
            "task-issue",
            77,
            "https://github.com/owner/repo/pull/77",
        )
        .await?;

    let issue_number = resolve_runtime_feedback_issue_number(
        Some(&store),
        project_root.path(),
        Some("owner/repo"),
        None,
        77,
    )
    .await;

    assert_eq!(issue_number, Some(123));
    Ok(())
}

#[test]
fn review_bot_author_matching_uses_configured_reviewer_name() {
    let mut config = review_config_with_chain(&["gemini", "codex"]);
    config.reviewer_name = "custom-reviewer".to_string();
    let chain = bot_fallback_chain(&config);

    assert_eq!(
        review_bot_key_for_author(&chain, "custom-reviewer"),
        Some(ReviewBotKey::Gemini)
    );
    assert_eq!(
        review_bot_key_for_author(&chain, CODEX_REVIEWER_NAME),
        Some(ReviewBotKey::Codex)
    );
    assert_eq!(
        review_bot_key_for_author(&chain, "unconfigured-reviewer[bot]"),
        None
    );
}

#[test]
fn comment_signal_marks_prior_bot_participation() {
    let mut bot = bot_signals();
    let comment_at = Utc::now() - chrono::Duration::minutes(60);

    record_bot_comment_signal(&mut bot, "previous review comment".to_string(), comment_at);

    assert!(bot.reviewed_any_commit);
    assert_eq!(bot.latest_comment_at, Some(comment_at));
    assert_eq!(
        bot.latest_comment_body.as_deref(),
        Some("previous review comment")
    );
}

#[test]
fn stale_comment_participation_can_satisfy_silence_fallback() {
    let mut bot = bot_signals();
    let latest_commit_at = Utc::now() - chrono::Duration::minutes(45);
    record_bot_comment_signal(
        &mut bot,
        "previous review comment".to_string(),
        Utc::now() - chrono::Duration::minutes(60),
    );

    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot,
        latest_commit_at,
        true,
        false,
        3,
        3,
        30,
    );

    assert_eq!(classification, BotClassification::Silent);
}

fn signals_with(primary: BotSignals, secondary: BotSignals) -> PullRequestSignals {
    let latest_commit_at = Utc::now() - chrono::Duration::minutes(45);
    let mut bots = HashMap::new();
    bots.insert(ReviewBotKey::Gemini, primary);
    bots.insert(ReviewBotKey::Codex, secondary);
    PullRequestSignals {
        latest_commit_at,
        ci_state: Some("SUCCESS".to_string()),
        ci_green: true,
        latest_bot_activity_at: None,
        blocking_feedback: false,
        bots,
    }
}

#[test]
fn classifies_gemini_quota_body() {
    let mut bot = bot_signals();
    bot.reviewed_any_commit = true;
    bot.latest_comment_at = Some(Utc::now());
    bot.latest_comment_body =
        Some("You have reached your daily quota limit for Gemini Code Assist.".to_string());
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Gemini),
        &bot,
        Utc::now() - chrono::Duration::minutes(5),
        true,
        false,
        0,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::QuotaExhausted);
}

#[test]
fn classifies_codex_quota_body() {
    let mut bot = bot_signals();
    bot.reviewed_any_commit = true;
    bot.latest_comment_at = Some(Utc::now());
    bot.latest_comment_body =
        Some("Codex usage limits have been reached for this account.".to_string());
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot,
        Utc::now() - chrono::Duration::minutes(5),
        true,
        false,
        0,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::QuotaExhausted);
}

#[test]
fn decides_tier_c_when_both_bots_are_quota_exhausted() {
    let mut gemini = bot_signals();
    gemini.reviewed_any_commit = true;
    gemini.latest_comment_at = Some(Utc::now());
    gemini.latest_comment_body =
        Some("You have reached your daily quota limit for Gemini Code Assist.".to_string());
    let mut codex = bot_signals();
    codex.reviewed_any_commit = true;
    codex.latest_comment_at = Some(Utc::now());
    codex.latest_comment_body =
        Some("Codex usage limits have been reached for this account.".to_string());
    let decision = decide_review_loop_action(
        &bot_fallback_chain(&review_config()),
        &signals_with(gemini, codex),
        0,
        &review_config(),
    )
    .expect("decision");
    assert_eq!(decision.active_bot.key, ReviewBotKey::Codex);
    let fallback = decision.fallback.expect("fallback");
    assert_eq!(
        fallback.tier,
        harness_workflow::issue_lifecycle::ReviewFallbackTier::C
    );
    assert_eq!(
        fallback.trigger,
        harness_workflow::issue_lifecycle::ReviewFallbackTrigger::AllBotsQuota
    );
}

#[test]
fn classifies_silence_when_all_guards_are_met() {
    let mut bot = bot_signals();
    bot.reviewed_any_commit = true;
    bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
    bot.latest_review_state = Some("COMMENTED".to_string());
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot,
        Utc::now() - chrono::Duration::minutes(45),
        true,
        false,
        3,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::Silent);
}

#[test]
fn silence_rounds_reset_on_new_bot_activity() {
    let initial_activity = Some(Utc::now() - chrono::Duration::minutes(10));
    let new_activity = Some(Utc::now());
    assert_eq!(
        update_silence_rounds(2, initial_activity, new_activity),
        (0, new_activity)
    );
}

#[test]
fn silence_is_blocked_by_red_ci() {
    let mut bot = bot_signals();
    bot.reviewed_any_commit = true;
    bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot,
        Utc::now() - chrono::Duration::minutes(45),
        false,
        false,
        3,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::ActionableFeedback);
}

#[test]
fn silence_is_blocked_by_unresolved_high_feedback() {
    let mut bot = bot_signals();
    bot.reviewed_any_commit = true;
    bot.latest_review_at = Some(Utc::now() - chrono::Duration::minutes(60));
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot,
        Utc::now() - chrono::Duration::minutes(45),
        true,
        true,
        3,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::ActionableFeedback);
}

#[test]
fn silence_is_blocked_when_no_prior_bot_review_exists() {
    let classification = classify_bot(
        &descriptor(ReviewBotKey::Codex),
        &bot_signals(),
        Utc::now() - chrono::Duration::minutes(45),
        true,
        false,
        3,
        3,
        30,
    );
    assert_eq!(classification, BotClassification::NeverReviewed);
}

#[test]
fn normal_fresh_gemini_path_stays_primary() {
    let mut gemini = bot_signals();
    gemini.reviewed_any_commit = true;
    gemini.latest_review_at = Some(Utc::now());
    gemini.latest_review_state = Some("APPROVED".to_string());
    let decision = decide_review_loop_action(
        &bot_fallback_chain(&review_config()),
        &signals_with(gemini, bot_signals()),
        0,
        &review_config(),
    )
    .expect("decision");
    assert_eq!(decision.active_bot.key, ReviewBotKey::Gemini);
    assert!(decision.fallback.is_none());
    assert!(!decision.wait_for_bot);
}

#[test]
fn respects_configured_secondary_order() {
    let mut codex = bot_signals();
    codex.reviewed_any_commit = true;
    codex.latest_comment_at = Some(Utc::now());
    codex.latest_comment_body =
        Some("Codex usage limits have been reached for this account.".to_string());
    let decision = decide_review_loop_action(
        &bot_fallback_chain(&review_config_with_chain(&["codex", "gemini"])),
        &signals_with(bot_signals(), codex),
        0,
        &review_config_with_chain(&["codex", "gemini"]),
    )
    .expect("decision");
    assert_eq!(decision.active_bot.key, ReviewBotKey::Gemini);
    let fallback = decision.fallback.expect("fallback");
    assert_eq!(
        fallback.tier,
        harness_workflow::issue_lifecycle::ReviewFallbackTier::B
    );
    assert_eq!(
        fallback.trigger,
        harness_workflow::issue_lifecycle::ReviewFallbackTrigger::CodexQuota
    );
    assert!(decision.wait_for_bot);
}
