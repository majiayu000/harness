use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::WorkflowActivityPolicy;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowClassifierPolicy {
    pub verdicts: Vec<String>,
    pub environment: Vec<String>,
    pub hard_deny: Vec<String>,
    pub soft_deny: Vec<String>,
    pub allow: Vec<String>,
}

pub(super) fn validate_classifier_activities(
    activities: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<()> {
    for (activity, policy) in activities {
        let Some(classifier) = policy.classifier.as_ref() else {
            continue;
        };
        if !policy.validation.is_empty() {
            anyhow::bail!(
                "classifier activity `{activity}` cannot declare repository validation commands"
            );
        }
        classifier.validate(activity)?;
    }
    Ok(())
}

impl WorkflowClassifierPolicy {
    pub fn validate(&self, activity: &str) -> anyhow::Result<()> {
        if self.verdicts.is_empty() {
            anyhow::bail!("classifier activity `{activity}` must declare at least one verdict");
        }
        let mut verdicts = BTreeSet::new();
        for verdict in &self.verdicts {
            let verdict = verdict.trim();
            if verdict.is_empty() {
                anyhow::bail!("classifier activity `{activity}` contains an empty verdict");
            }
            if !verdicts.insert(verdict) {
                anyhow::bail!(
                    "classifier activity `{activity}` declares duplicate verdict `{verdict}`"
                );
            }
        }
        if self.environment.is_empty()
            && self.hard_deny.is_empty()
            && self.soft_deny.is_empty()
            && self.allow.is_empty()
        {
            anyhow::bail!(
                "classifier activity `{activity}` must declare at least one environment or decision rule"
            );
        }
        for (section, rules) in [
            ("environment", &self.environment),
            ("hard_deny", &self.hard_deny),
            ("soft_deny", &self.soft_deny),
            ("allow", &self.allow),
        ] {
            if rules.iter().any(|rule| rule.trim().is_empty()) {
                anyhow::bail!(
                    "classifier activity `{activity}` contains an empty `{section}` rule"
                );
            }
        }
        Ok(())
    }

    pub fn validate_routes(
        &self,
        activity: &str,
        on_success: Option<&str>,
        on_failure: Option<&str>,
        on_signal: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        if on_success.is_some() {
            anyhow::bail!(
                "classifier activity `{activity}` must route only through declared verdict signals, not on_success"
            );
        }
        if on_failure != Some("blocked") {
            anyhow::bail!("classifier activity `{activity}` must route on_failure to `blocked`");
        }
        let verdicts = self
            .verdicts
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let routes = on_signal
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if verdicts != routes {
            anyhow::bail!(
                "classifier activity `{activity}` signal routes must exactly match classifier verdicts; verdicts={verdicts:?}, routes={routes:?}"
            );
        }
        Ok(())
    }
}
