use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::WorkflowActivityPolicy;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowClassifierPolicy {
    pub verdicts: Vec<String>,
    pub instructions: Vec<String>,
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
        if self.instructions.is_empty() {
            anyhow::bail!("classifier activity `{activity}` must declare at least one instruction");
        }
        if self
            .instructions
            .iter()
            .any(|instruction| instruction.trim().is_empty())
        {
            anyhow::bail!("classifier activity `{activity}` contains an empty instruction");
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
                "classifier activity `{activity}` must route only through declared verdicts, not on_success"
            );
        }
        if on_failure.is_none() {
            anyhow::bail!("classifier activity `{activity}` must declare on_failure");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> WorkflowClassifierPolicy {
        WorkflowClassifierPolicy {
            verdicts: vec!["allow".to_string(), "needs_human".to_string()],
            instructions: vec!["Judge only the supplied facts.".to_string()],
        }
    }

    #[test]
    fn rejects_duplicate_verdicts() {
        let mut policy = policy();
        policy.verdicts.push("allow".to_string());
        assert!(policy.validate("classify").is_err());
    }

    #[test]
    fn routes_must_exactly_match_verdicts() {
        let routes = BTreeMap::from([("allow".to_string(), "done".to_string())]);
        assert!(policy()
            .validate_routes("classify", None, Some("blocked"), &routes)
            .is_err());
    }
}
