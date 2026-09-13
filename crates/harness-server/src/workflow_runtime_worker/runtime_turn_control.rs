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

/// Authority A: `RuntimeKind` alone selects the execution surface.
/// Approval policy configures the selected agent; it must not switch
/// oneshot ↔ turn. Only `CodexExec` forces oneshot for the shared `"codex"`
/// agent name (which also registers a turn factory for `CodexJsonrpc`).
pub(super) fn force_oneshot_surface(runtime_kind: RuntimeKind) -> bool {
    matches!(runtime_kind, RuntimeKind::CodexExec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_kind_selects_surface_independent_of_approval() {
        for policy in [None, Some("never"), Some("on-request"), Some("untrusted")] {
            assert!(
                force_oneshot_surface(RuntimeKind::CodexExec),
                "CodexExec stays oneshot for approval={policy:?}"
            );
            assert!(
                !force_oneshot_surface(RuntimeKind::CodexJsonrpc),
                "CodexJsonrpc stays turn for approval={policy:?}"
            );
        }
        assert!(!force_oneshot_surface(RuntimeKind::ClaudeCode));
        assert!(!force_oneshot_surface(RuntimeKind::OpenCode));
    }
}
