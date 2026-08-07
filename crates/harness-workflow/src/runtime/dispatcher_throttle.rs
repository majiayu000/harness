//! Daily-cap throttle band for the pre-dispatch budget gate (GH-1770 §4.1).
//!
//! Between `daily_profile_cap_usd * daily_throttle_ratio` and the cap itself a
//! runtime profile is not blocked — it is deprioritized: its commands yield the
//! dispatch slot while work from a profile under its own threshold is
//! claimable. When no such alternative exists the throttled command dispatches,
//! so a single busy profile still makes progress instead of starving.

use super::dispatcher::RuntimeProfileSelector;
use super::model::WorkflowCommandRecord;
use super::store::{cost_usd_from_micros, cost_usd_to_micros, WorkflowRuntimeStore};
use chrono::{DateTime, Utc};
use harness_core::config::workflow::RuntimeBudgetPolicy;
use serde_json::{json, Value};
use std::collections::HashMap;

/// A throttle-band breach: the profile is inside its daily band and another
/// profile under its own threshold has claimable work.
pub(super) struct ThrottleBreach {
    pub(super) reason: String,
    pub(super) evidence: Value,
}

/// Throttle-band decision for one claimed command. `None` means dispatch may
/// proceed — either the profile is under its threshold or nothing better is
/// claimable, in which case a lone busy profile is slowed, never starved.
pub(super) struct ThrottleBandRequest<'a> {
    pub(super) store: &'a WorkflowRuntimeStore,
    pub(super) profile_selector: &'a RuntimeProfileSelector,
    pub(super) budget_policy: &'a RuntimeBudgetPolicy,
    pub(super) runtime_profile_name: &'a str,
    pub(super) profile_spent_usd_micros: u64,
    pub(super) cap_usd: f64,
    pub(super) utc_day_start: DateTime<Utc>,
    pub(super) peek_limit: i64,
}

pub(super) async fn daily_throttle_breach(
    request: ThrottleBandRequest<'_>,
) -> anyhow::Result<Option<ThrottleBreach>> {
    let ThrottleBandRequest {
        store,
        profile_selector,
        budget_policy,
        runtime_profile_name,
        profile_spent_usd_micros,
        cap_usd,
        utc_day_start,
        peek_limit,
    } = request;
    let Some(threshold_usd) = budget_policy.daily_throttle_threshold_usd() else {
        return Ok(None);
    };
    let threshold_usd_micros = cost_usd_to_micros(threshold_usd)?;
    if profile_spent_usd_micros < threshold_usd_micros {
        return Ok(None);
    }
    let Some(alternative) = under_threshold_alternative_profile(
        store,
        profile_selector,
        runtime_profile_name,
        threshold_usd_micros,
        utc_day_start,
        peek_limit,
    )
    .await?
    else {
        return Ok(None);
    };
    let profile_spent_usd = cost_usd_from_micros(profile_spent_usd_micros);
    Ok(Some(ThrottleBreach {
        reason: format!(
            "runtime profile {runtime_profile_name} spent {profile_spent_usd:.2} USD today, \
             inside the throttle band of its {cap_usd:.2} USD daily cap; runtime profile \
             {alternative} is under its threshold and goes first"
        ),
        evidence: json!({
            "runtime_profile": runtime_profile_name,
            "profile_spent_usd_today": profile_spent_usd,
            "daily_profile_cap_usd": cap_usd,
            "daily_throttle_threshold_usd": threshold_usd,
            "yielded_to_runtime_profile": alternative,
        }),
    }))
}

/// The runtime profile of some other claimable command that is under its daily
/// throttle threshold, if any. `None` means nothing better is waiting and the
/// throttled command may dispatch.
async fn under_threshold_alternative_profile(
    store: &WorkflowRuntimeStore,
    profile_selector: &RuntimeProfileSelector,
    throttled_profile: &str,
    threshold_usd_micros: u64,
    utc_day_start: DateTime<Utc>,
    peek_limit: i64,
) -> anyhow::Result<Option<String>> {
    let claimable = store.peek_claimable_commands(peek_limit).await?;
    let mut definition_ids: HashMap<String, Option<String>> = HashMap::new();
    let mut profile_spend: HashMap<String, u64> = HashMap::new();

    for command in &claimable {
        let profile =
            candidate_profile_name(store, profile_selector, command, &mut definition_ids).await?;
        if profile == throttled_profile {
            continue;
        }
        let spent_usd_micros = match profile_spend.get(&profile) {
            Some(spent) => *spent,
            None => {
                let spent = store
                    .runtime_usage_cost_for_profile_since(&profile, utc_day_start)
                    .await?;
                profile_spend.insert(profile.clone(), spent);
                spent
            }
        };
        if spent_usd_micros < threshold_usd_micros {
            return Ok(Some(profile));
        }
    }
    Ok(None)
}

/// Resolve the runtime profile a claimable command would dispatch under,
/// memoizing the workflow lookup so a batch of commands from one workflow costs
/// a single instance read.
async fn candidate_profile_name(
    store: &WorkflowRuntimeStore,
    profile_selector: &RuntimeProfileSelector,
    command: &WorkflowCommandRecord,
    definition_ids: &mut HashMap<String, Option<String>>,
) -> anyhow::Result<String> {
    let definition_id = match definition_ids.get(&command.workflow_id) {
        Some(definition_id) => definition_id.clone(),
        None => {
            let definition_id = store
                .get_instance(&command.workflow_id)
                .await?
                .map(|instance| instance.definition_id);
            definition_ids.insert(command.workflow_id.clone(), definition_id.clone());
            definition_id
        }
    };
    Ok(profile_selector
        .select(
            definition_id.as_deref(),
            Some(command.command.runtime_activity_key()),
        )
        .name
        .clone())
}
