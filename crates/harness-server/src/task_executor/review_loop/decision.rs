use super::signals::{
    classify_bot, quota_trigger, BotClassification, PullRequestSignals, ReviewBotDescriptor,
    ReviewFallbackState, ReviewLoopDecision,
};
use chrono::{DateTime, Utc};

pub(super) fn decide_review_loop_action(
    chain: &[ReviewBotDescriptor],
    signals: &PullRequestSignals,
    silence_rounds: u32,
    review_config: &harness_core::config::agents::AgentReviewConfig,
) -> anyhow::Result<ReviewLoopDecision> {
    let primary = chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("review fallback chain is empty"))?;
    let primary_signals = signals.bots.get(&primary.key).cloned().unwrap_or_default();
    let primary_classification = classify_bot(
        primary,
        &primary_signals,
        signals.latest_commit_at,
        signals.ci_green,
        signals.blocking_feedback,
        silence_rounds,
        review_config.silence_rounds_threshold,
        review_config.silence_min_minutes_after_commit,
    );
    if primary_classification != BotClassification::QuotaExhausted {
        return Ok(ReviewLoopDecision {
            active_bot: primary.clone(),
            fallback: None,
            wait_for_bot: primary_classification == BotClassification::NeverReviewed,
        });
    }

    let secondary = chain.get(1).cloned().unwrap_or_else(|| primary.clone());
    let secondary_key = secondary.key;
    let secondary_signals = signals
        .bots
        .get(&secondary.key)
        .cloned()
        .unwrap_or_default();
    let secondary_classification = classify_bot(
        &secondary,
        &secondary_signals,
        signals.latest_commit_at,
        signals.ci_green,
        signals.blocking_feedback,
        silence_rounds,
        review_config.silence_rounds_threshold,
        review_config.silence_min_minutes_after_commit,
    );
    let fallback_b = Some(ReviewFallbackState {
        tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::B,
        trigger: quota_trigger(primary.key),
        active_bot: Some(secondary.key),
    });
    match secondary_classification {
        BotClassification::QuotaExhausted => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: Some(ReviewFallbackState {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::AllBotsQuota,
                active_bot: Some(secondary_key),
            }),
            wait_for_bot: false,
        }),
        BotClassification::Silent => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: Some(ReviewFallbackState {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
                active_bot: Some(secondary_key),
            }),
            wait_for_bot: false,
        }),
        BotClassification::NeverReviewed => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: fallback_b,
            wait_for_bot: true,
        }),
        _ => Ok(ReviewLoopDecision {
            active_bot: secondary,
            fallback: fallback_b,
            wait_for_bot: false,
        }),
    }
}

pub(super) fn update_silence_rounds(
    current_rounds: u32,
    last_activity_at: Option<DateTime<Utc>>,
    next_activity_at: Option<DateTime<Utc>>,
) -> (u32, Option<DateTime<Utc>>) {
    if next_activity_at != last_activity_at {
        (0, next_activity_at)
    } else {
        (current_rounds.saturating_add(1), last_activity_at)
    }
}
