//! Fail-closed dispatch gate for pinned agent contracts.
//!
//! Until contract enforcement ships, no runtime profile can prove the
//! no-tool/no-mutation/empty-workspace constraints an `agent_contract`
//! declares. Dispatching such a command through the ordinary executor would
//! silently ignore every constraint, so the dispatcher defers it behind an
//! explicit barrier instead. The barrier lifts once enforcement exists and
//! this gate checks the effective runtime capability instead of refusing.

use super::{
    command_project_id, dispatch_deferral_outcome, CommandDispatchOutcome, RuntimeCommandDispatcher,
};
use crate::runtime::model::{WorkflowCommandRecord, WorkflowInstance};
use crate::runtime::{DispatchBarrierInput, DispatchBarrierReasonCode};
use chrono::Utc;

impl RuntimeCommandDispatcher<'_> {
    pub(super) async fn defer_unenforceable_agent_contract(
        &self,
        command: &WorkflowCommandRecord,
        instance: Option<&WorkflowInstance>,
        activity: &str,
    ) -> anyhow::Result<CommandDispatchOutcome> {
        let project_id = command_project_id(instance, command)?;
        let barrier = DispatchBarrierInput::new(
            DispatchBarrierReasonCode::AgentContractEnforcementUnavailable,
            format!(
                "activity '{activity}' declares an agent_contract, but no configured runtime can enforce it yet"
            ),
            project_id,
        );
        dispatch_deferral_outcome(
            command,
            self.store
                .defer_claimed_command_if_owned(
                    &command.id,
                    &self.dispatcher_id,
                    command.dispatch_claim_generation,
                    barrier,
                    Utc::now(),
                    self.defer_backoff,
                )
                .await?,
        )
    }
}
