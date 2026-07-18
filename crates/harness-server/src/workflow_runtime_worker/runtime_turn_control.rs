use harness_core::types::TurnId;
use harness_workflow::runtime::RuntimeKind;
use std::sync::Arc;

pub(super) struct RuntimeTurnAliasGuard {
    server: Arc<crate::server::HarnessServer>,
    alias: String,
    turn_id: TurnId,
}

impl RuntimeTurnAliasGuard {
    pub(super) fn register(
        server: Arc<crate::server::HarnessServer>,
        alias: String,
        turn_id: TurnId,
    ) -> Self {
        server
            .thread_manager
            .register_runtime_turn_alias(&alias, &turn_id);
        Self {
            server,
            alias,
            turn_id,
        }
    }
}

impl Drop for RuntimeTurnAliasGuard {
    fn drop(&mut self) {
        self.server
            .thread_manager
            .deregister_runtime_turn_alias(&self.alias, &self.turn_id);
    }
}

pub(super) fn force_code_agent_for_runtime_turn(
    runtime_kind: RuntimeKind,
    approval_policy: Option<&str>,
) -> bool {
    matches!(
        runtime_kind,
        RuntimeKind::CodexExec | RuntimeKind::CodexJsonrpc
    ) && !matches!(
        approval_policy,
        Some("untrusted" | "on-failure" | "on-request")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_codex_approval_uses_turn_adapter() {
        for policy in ["untrusted", "on-failure", "on-request"] {
            assert!(!force_code_agent_for_runtime_turn(
                RuntimeKind::CodexExec,
                Some(policy)
            ));
        }
    }

    #[test]
    fn non_interactive_codex_keeps_exec_path() {
        assert!(force_code_agent_for_runtime_turn(
            RuntimeKind::CodexExec,
            None
        ));
        assert!(force_code_agent_for_runtime_turn(
            RuntimeKind::CodexJsonrpc,
            Some("never")
        ));
        assert!(!force_code_agent_for_runtime_turn(
            RuntimeKind::ClaudeCode,
            Some("on-request")
        ));
    }
}
