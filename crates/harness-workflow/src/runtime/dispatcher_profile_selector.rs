//! Runtime profile selection for command dispatch, split from
//! `dispatcher.rs` to keep that module within size limits.

use crate::runtime::model::RuntimeProfile;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProfileSelector {
    default_profile: RuntimeProfile,
    workflow_profiles: BTreeMap<String, RuntimeProfile>,
    activity_profiles: BTreeMap<String, RuntimeProfile>,
    workflow_activity_profiles: BTreeMap<String, BTreeMap<String, RuntimeProfile>>,
}

impl RuntimeProfileSelector {
    pub fn new(default_profile: RuntimeProfile) -> Self {
        Self {
            default_profile,
            workflow_profiles: BTreeMap::new(),
            activity_profiles: BTreeMap::new(),
            workflow_activity_profiles: BTreeMap::new(),
        }
    }

    pub fn with_workflow_profile(
        mut self,
        definition_id: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.workflow_profiles.insert(definition_id.into(), profile);
        self
    }

    pub fn with_activity_profile(
        mut self,
        activity: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.activity_profiles.insert(activity.into(), profile);
        self
    }

    pub fn with_workflow_activity_profile(
        mut self,
        definition_id: impl Into<String>,
        activity: impl Into<String>,
        profile: RuntimeProfile,
    ) -> Self {
        self.workflow_activity_profiles
            .entry(definition_id.into())
            .or_default()
            .insert(activity.into(), profile);
        self
    }

    pub fn select(&self, definition_id: Option<&str>, activity: Option<&str>) -> &RuntimeProfile {
        definition_id
            .and_then(|id| {
                activity.and_then(|name| {
                    self.workflow_activity_profiles
                        .get(id)
                        .and_then(|profiles| profiles.get(name))
                })
            })
            .or_else(|| activity.and_then(|name| self.activity_profiles.get(name)))
            .or_else(|| definition_id.and_then(|id| self.workflow_profiles.get(id)))
            .unwrap_or(&self.default_profile)
    }

    pub fn select_for_workflow(&self, definition_id: Option<&str>) -> &RuntimeProfile {
        definition_id
            .and_then(|id| self.workflow_profiles.get(id))
            .unwrap_or(&self.default_profile)
    }
}

impl From<RuntimeProfile> for RuntimeProfileSelector {
    fn from(default_profile: RuntimeProfile) -> Self {
        Self::new(default_profile)
    }
}
