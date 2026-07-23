use harness_core::compress::NapStatus;
use harness_core::run_id::RunId;
use harness_core::types::{ProjectId, ThreadId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPreviewRequest {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub project: ProjectId,
    pub task_profile: ContextPreviewTaskProfile,
    pub budget_hint: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextPreviewTaskProfile {
    #[serde(default)]
    pub task_kind: Option<String>,
    #[serde(default)]
    pub target_paths: Vec<PathBuf>,
    #[serde(default)]
    pub agent_kind: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub contract: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextPreviewItemId(String);

impl ContextPreviewItemId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ContextPreviewItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPreviewItemClass {
    Rule,
    Skill,
    Contract,
    Brief,
    Draft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPreviewPriority {
    P0,
    P1,
    P2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "level", content = "content", rename_all = "snake_case")]
pub enum ContextPreviewDegraded {
    Summary(String),
    Pointer(String),
    Summarized { text: String, nap: NapStatus },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPreviewItem {
    pub id: ContextPreviewItemId,
    pub class: ContextPreviewItemClass,
    pub content: String,
    pub est_tokens: u32,
    pub priority: ContextPreviewPriority,
    pub relevance: f32,
    #[serde(default)]
    pub degrade: Vec<ContextPreviewDegraded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub instruction_bearing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::methods::{Method, RpcRequest};

    fn request_with_all_fields() -> ContextPreviewRequest {
        ContextPreviewRequest {
            thread_id: ThreadId::from_str("thread-golden"),
            run_id: Some(
                "ar-01j1qb3c9r7v5m2k8x4tznq6wf"
                    .parse()
                    .expect("valid run id"),
            ),
            project: ProjectId::from_str("project-golden"),
            task_profile: ContextPreviewTaskProfile {
                task_kind: Some("implementation".to_string()),
                target_paths: vec!["src/lib.rs".into(), "tests/wire.rs".into()],
                agent_kind: Some("codex".to_string()),
                prompt: Some("Preserve the wire contract.".to_string()),
                contract: Some("typed-boundary".to_string()),
            },
            budget_hint: 4096,
        }
    }

    #[test]
    fn context_preview_item_id_accessors_preserve_value() {
        let id = ContextPreviewItemId::new("rule:accessors");
        assert_eq!(id.as_str(), "rule:accessors");
        assert_eq!(id.into_inner(), "rule:accessors");
    }

    fn item(
        id: &str,
        class: ContextPreviewItemClass,
        priority: ContextPreviewPriority,
        relevance: f32,
        degrade: Vec<ContextPreviewDegraded>,
        dedupe_key: Option<&str>,
    ) -> ContextPreviewItem {
        ContextPreviewItem {
            id: ContextPreviewItemId::new(id),
            class,
            content: format!("content for {id}"),
            est_tokens: 17,
            priority,
            relevance,
            degrade,
            dedupe_key: dedupe_key.map(str::to_string),
            instruction_bearing: true,
        }
    }

    #[test]
    fn context_preview_wire_json_matches_legacy_golden_fixtures() {
        let request = request_with_all_fields();
        let supplied_items = vec![
            item(
                "rule:golden",
                ContextPreviewItemClass::Rule,
                ContextPreviewPriority::P0,
                1.0,
                vec![
                    ContextPreviewDegraded::Summary("rule summary".to_string()),
                    ContextPreviewDegraded::Pointer("rule pointer".to_string()),
                    ContextPreviewDegraded::Summarized {
                        text: "verified summary".to_string(),
                        nap: NapStatus::Verified,
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "sample skipped".to_string(),
                        nap: NapStatus::SkippedSample,
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "fell back".to_string(),
                        nap: NapStatus::Failed { fell_back: true },
                    },
                    ContextPreviewDegraded::Summarized {
                        text: "did not fall back".to_string(),
                        nap: NapStatus::Failed { fell_back: false },
                    },
                ],
                Some("rule-dedupe"),
            ),
            item(
                "skill:golden",
                ContextPreviewItemClass::Skill,
                ContextPreviewPriority::P1,
                0.75,
                Vec::new(),
                None,
            ),
            item(
                "contract:golden",
                ContextPreviewItemClass::Contract,
                ContextPreviewPriority::P2,
                0.5,
                Vec::new(),
                None,
            ),
            item(
                "brief:golden",
                ContextPreviewItemClass::Brief,
                ContextPreviewPriority::P1,
                0.25,
                Vec::new(),
                None,
            ),
            item(
                "draft:golden",
                ContextPreviewItemClass::Draft,
                ContextPreviewPriority::P2,
                0.0,
                Vec::new(),
                None,
            ),
        ];

        let actual = serde_json::to_value(serde_json::json!({
            "request": request,
            "supplied_items": supplied_items,
        }))
        .expect("serialize fixture");
        let expected = serde_json::json!({
            "request": {
                "thread_id": "thread-golden",
                "run_id": "ar-01j1qb3c9r7v5m2k8x4tznq6wf",
                "project": "project-golden",
                "task_profile": {
                    "task_kind": "implementation",
                    "target_paths": ["src/lib.rs", "tests/wire.rs"],
                    "agent_kind": "codex",
                    "prompt": "Preserve the wire contract.",
                    "contract": "typed-boundary"
                },
                "budget_hint": 4096
            },
            "supplied_items": [
                {
                    "id": "rule:golden",
                    "class": "rule",
                    "content": "content for rule:golden",
                    "est_tokens": 17,
                    "priority": "p0",
                    "relevance": 1.0,
                    "degrade": [
                        {"level": "summary", "content": "rule summary"},
                        {"level": "pointer", "content": "rule pointer"},
                        {
                            "level": "summarized",
                            "content": {
                                "text": "verified summary",
                                "nap": {"status": "verified"}
                            }
                        },
                        {
                            "level": "summarized",
                            "content": {
                                "text": "sample skipped",
                                "nap": {"status": "skipped_sample"}
                            }
                        },
                        {
                            "level": "summarized",
                            "content": {
                                "text": "fell back",
                                "nap": {"status": "failed", "fell_back": true}
                            }
                        },
                        {
                            "level": "summarized",
                            "content": {
                                "text": "did not fall back",
                                "nap": {"status": "failed", "fell_back": false}
                            }
                        }
                    ],
                    "dedupe_key": "rule-dedupe",
                    "instruction_bearing": true
                },
                {
                    "id": "skill:golden",
                    "class": "skill",
                    "content": "content for skill:golden",
                    "est_tokens": 17,
                    "priority": "p1",
                    "relevance": 0.75,
                    "degrade": [],
                    "instruction_bearing": true
                },
                {
                    "id": "contract:golden",
                    "class": "contract",
                    "content": "content for contract:golden",
                    "est_tokens": 17,
                    "priority": "p2",
                    "relevance": 0.5,
                    "degrade": [],
                    "instruction_bearing": true
                },
                {
                    "id": "brief:golden",
                    "class": "brief",
                    "content": "content for brief:golden",
                    "est_tokens": 17,
                    "priority": "p1",
                    "relevance": 0.25,
                    "degrade": [],
                    "instruction_bearing": true
                },
                {
                    "id": "draft:golden",
                    "class": "draft",
                    "content": "content for draft:golden",
                    "est_tokens": 17,
                    "priority": "p2",
                    "relevance": 0.0,
                    "degrade": [],
                    "instruction_bearing": true
                }
            ]
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn context_preview_defaults_and_empty_collections_are_equivalent() {
        let omitted = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "context/preview",
            "params": {
                "request": {
                    "thread_id": "thread-defaults",
                    "project": "project-defaults",
                    "task_profile": {},
                    "budget_hint": 0
                }
            }
        });
        let explicit_empty = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "context/preview",
            "params": {
                "request": {
                    "thread_id": "thread-defaults",
                    "project": "project-defaults",
                    "task_profile": {"target_paths": []},
                    "budget_hint": 0
                },
                "supplied_items": []
            }
        });

        let omitted: RpcRequest = serde_json::from_value(omitted).expect("omitted defaults parse");
        let explicit_empty: RpcRequest =
            serde_json::from_value(explicit_empty).expect("explicit empty values parse");
        let (
            Method::ContextPreview {
                request: omitted_request,
                supplied_items: omitted_items,
            },
            Method::ContextPreview {
                request: explicit_request,
                supplied_items: explicit_items,
            },
        ) = (omitted.method, explicit_empty.method)
        else {
            panic!("expected context preview methods");
        };

        assert_eq!(omitted_request, explicit_request);
        assert_eq!(omitted_items, explicit_items);
        assert_eq!(
            serde_json::to_value(omitted_request).expect("serialize defaults"),
            serde_json::json!({
                "thread_id": "thread-defaults",
                "project": "project-defaults",
                "task_profile": {
                    "task_kind": null,
                    "target_paths": [],
                    "agent_kind": null,
                    "prompt": null,
                    "contract": null
                },
                "budget_hint": 0
            })
        );

        let item: ContextPreviewItem = serde_json::from_value(serde_json::json!({
            "id": "rule:none",
            "class": "rule",
            "content": "",
            "est_tokens": 0,
            "priority": "p2",
            "relevance": 0.0,
            "instruction_bearing": false
        }))
        .expect("item defaults parse");
        let explicit_empty_item: ContextPreviewItem = serde_json::from_value(serde_json::json!({
            "id": "rule:none",
            "class": "rule",
            "content": "",
            "est_tokens": 0,
            "priority": "p2",
            "relevance": 0.0,
            "degrade": [],
            "instruction_bearing": false
        }))
        .expect("explicit empty item parse");
        assert_eq!(item, explicit_empty_item);
        assert!(item.degrade.is_empty());
        assert_eq!(
            serde_json::to_value(item).expect("serialize item defaults"),
            serde_json::json!({
                "id": "rule:none",
                "class": "rule",
                "content": "",
                "est_tokens": 0,
                "priority": "p2",
                "relevance": 0.0,
                "degrade": [],
                "instruction_bearing": false
            })
        );
    }

    #[test]
    fn context_preview_rejects_malformed_payloads() {
        fn wire_with_item(item: serde_json::Value) -> serde_json::Value {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "context/preview",
                "params": {
                    "request": {
                        "thread_id": "thread-invalid",
                        "project": "project-invalid",
                        "task_profile": {},
                        "budget_hint": 100
                    },
                    "supplied_items": [item]
                }
            })
        }

        let valid_item = serde_json::json!({
            "id": "rule:invalid",
            "class": "rule",
            "content": "content",
            "est_tokens": 1,
            "priority": "p1",
            "relevance": 1.0,
            "degrade": [],
            "instruction_bearing": true
        });
        let mut malformed = Vec::new();
        for required_field in ["thread_id", "project", "task_profile", "budget_hint"] {
            let mut missing_required_request = wire_with_item(valid_item.clone());
            missing_required_request["params"]["request"]
                .as_object_mut()
                .expect("request object")
                .remove(required_field);
            malformed.push(missing_required_request);
        }
        for required_field in [
            "id",
            "class",
            "content",
            "est_tokens",
            "priority",
            "relevance",
            "instruction_bearing",
        ] {
            let mut missing_required_item = valid_item.clone();
            missing_required_item
                .as_object_mut()
                .expect("item object")
                .remove(required_field);
            malformed.push(wire_with_item(missing_required_item));
        }

        for (field, invalid_value) in [
            ("class", serde_json::json!("unknown")),
            ("priority", serde_json::json!("urgent")),
        ] {
            let mut item = valid_item.clone();
            item[field] = invalid_value;
            malformed.push(wire_with_item(item));
        }
        for invalid_degrade in [
            serde_json::json!({"level": "unknown", "content": "value"}),
            serde_json::json!({"level": "summarized", "content": {"text": "missing nap"}}),
            serde_json::json!({
                "level": "summarized",
                "content": {"text": "bad nap", "nap": {"status": "unknown"}}
            }),
        ] {
            let mut item = valid_item.clone();
            item["degrade"] = serde_json::json!([invalid_degrade]);
            malformed.push(wire_with_item(item));
        }

        for value in malformed {
            assert!(
                serde_json::from_value::<RpcRequest>(value).is_err(),
                "malformed request must fail deserialization"
            );
        }
    }
}
