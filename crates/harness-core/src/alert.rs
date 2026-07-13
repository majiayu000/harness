//! Outbound alert contract shared between producers and the server-side
//! alert dispatcher (GH-1582).
//!
//! The payload is a stable, additive-only JSON contract (`schema_version`):
//! fields may be added but never removed or renamed within a major version.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current alert payload schema version. Bump only on additive changes.
pub const ALERT_SCHEMA_VERSION: u32 = 1;

/// Closed set of alert event classes (product spec GH1582 B-003).
///
/// Config references these by their snake_case serde names; unknown class
/// strings in config fail deserialization instead of being ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertClass {
    TaskFailureExhausted,
    WorkflowBlocked,
    WorkflowFailed,
    CircuitBreakerOpen,
    ReconciliationAnomaly,
    ReadyToMergeAging,
    NotifyChannelDrop,
}

impl AlertClass {
    /// Stable snake_case name, identical to the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertClass::TaskFailureExhausted => "task_failure_exhausted",
            AlertClass::WorkflowBlocked => "workflow_blocked",
            AlertClass::WorkflowFailed => "workflow_failed",
            AlertClass::CircuitBreakerOpen => "circuit_breaker_open",
            AlertClass::ReconciliationAnomaly => "reconciliation_anomaly",
            AlertClass::ReadyToMergeAging => "ready_to_merge_aging",
            AlertClass::NotifyChannelDrop => "notify_channel_drop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Error,
}

/// Identifiers of the entity an alert is about (product spec B-020).
///
/// All fields are optional; producers fill what they know. `entity` is a
/// free-form fallback reference (e.g. a circuit-breaker scope key).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertSubject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
}

/// The outbound alert payload delivered to every configured channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertPayload {
    pub schema_version: u32,
    pub event_class: AlertClass,
    pub severity: AlertSeverity,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub subject: AlertSubject,
    pub message: String,
    pub dedup_key: String,
}

impl AlertPayload {
    /// Build a payload stamped with the current schema version and time.
    ///
    /// `dedup_key` should be built with [`AlertPayload::dedup_key_for`] so
    /// suppression windows group by `class:entity`.
    pub fn new(
        event_class: AlertClass,
        severity: AlertSeverity,
        message: impl Into<String>,
        dedup_key: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ALERT_SCHEMA_VERSION,
            event_class,
            severity,
            timestamp: Utc::now(),
            project: None,
            subject: AlertSubject::default(),
            message: message.into(),
            dedup_key: dedup_key.into(),
        }
    }

    /// Canonical dedup key: `<class>:<entity>`.
    pub fn dedup_key_for(class: AlertClass, entity: &str) -> String {
        format!("{}:{entity}", class.as_str())
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    pub fn with_subject(mut self, subject: AlertSubject) -> Self {
        self.subject = subject;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_class_serializes_snake_case() {
        for (class, expected) in [
            (AlertClass::TaskFailureExhausted, "task_failure_exhausted"),
            (AlertClass::WorkflowBlocked, "workflow_blocked"),
            (AlertClass::WorkflowFailed, "workflow_failed"),
            (AlertClass::CircuitBreakerOpen, "circuit_breaker_open"),
            (AlertClass::ReconciliationAnomaly, "reconciliation_anomaly"),
            (AlertClass::ReadyToMergeAging, "ready_to_merge_aging"),
            (AlertClass::NotifyChannelDrop, "notify_channel_drop"),
        ] {
            let json = serde_json::to_string(&class).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            assert_eq!(class.as_str(), expected);
            let back: AlertClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, class);
        }
    }

    #[test]
    fn unknown_class_string_fails_deserialization() {
        let result = serde_json::from_str::<AlertClass>("\"made_up_class\"");
        assert!(
            result.is_err(),
            "unknown class must be rejected, not ignored"
        );
    }

    /// B-005: the top-level JSON field set is a stable contract.
    #[test]
    fn payload_json_field_set_is_stable() {
        let payload = AlertPayload::new(
            AlertClass::TaskFailureExhausted,
            AlertSeverity::Error,
            "task t-1 exhausted retries",
            AlertPayload::dedup_key_for(AlertClass::TaskFailureExhausted, "t-1"),
        )
        .with_project("harness")
        .with_subject(AlertSubject {
            task_id: Some("t-1".into()),
            ..AlertSubject::default()
        });

        let value = serde_json::to_value(&payload).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "dedup_key",
                "event_class",
                "message",
                "project",
                "schema_version",
                "severity",
                "subject",
                "timestamp",
            ],
        );
        assert_eq!(object["schema_version"], 1);
        assert_eq!(object["event_class"], "task_failure_exhausted");
        assert_eq!(object["severity"], "error");
        assert_eq!(object["dedup_key"], "task_failure_exhausted:t-1");
        assert_eq!(object["subject"]["task_id"], "t-1");
    }

    #[test]
    fn dedup_key_is_class_colon_entity() {
        assert_eq!(
            AlertPayload::dedup_key_for(AlertClass::CircuitBreakerOpen, "repo:main"),
            "circuit_breaker_open:repo:main"
        );
    }
}
