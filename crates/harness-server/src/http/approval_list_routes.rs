use super::{rest_contract::ContractJson, state::AppState};
use axum::extract::State;
use harness_protocol::rest::{PendingRuntimeApproval, PendingRuntimeApprovalsResponse};
use std::sync::Arc;

pub(crate) async fn list_pending_approvals(
    State(state): State<Arc<AppState>>,
) -> ContractJson<PendingRuntimeApprovalsResponse> {
    let data = state
        .core
        .server
        .thread_manager
        .pending_runtime_approvals()
        .into_iter()
        .map(
            |(submission_id, pending_approvals)| PendingRuntimeApproval {
                submission_id,
                pending_approvals,
            },
        )
        .collect::<Vec<_>>();
    ContractJson(PendingRuntimeApprovalsResponse { data })
}
