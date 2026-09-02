use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    RuntimeKind, WorkflowCommand, WorkflowDefinitionRegistry, WorkflowRuntimeStore,
    WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID,
};

fn workflow(state: &str, data: Value) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("issue", "issue:1"),
    )
    .with_server_data(data)
}

include!("health_sampling_cases.rs");
include!("action_cases.rs");
include!("declarative_cases.rs");
