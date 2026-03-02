use crate::server::HarnessServer;
use harness_protocol::{Method, RpcRequest, RpcResponse, INTERNAL_ERROR, METHOD_NOT_FOUND};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(server: &HarnessServer, req: RpcRequest) -> RpcResponse {
    let id = req.id.clone();

    match req.method {
        Method::Initialize => {
            RpcResponse::success(id, serde_json::json!({
                "name": "harness",
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": {
                    "threads": true,
                    "gc": true,
                    "skills": true,
                    "rules": true,
                    "exec_plan": true,
                    "observe": true,
                }
            }))
        }

        Method::ThreadStart { cwd } => {
            let thread_id = server.thread_manager.start_thread(cwd);
            RpcResponse::success(id, serde_json::json!({ "thread_id": thread_id }))
        }

        Method::ThreadList => {
            let threads = server.thread_manager.list_threads();
            RpcResponse::success(id, serde_json::to_value(threads).unwrap_or_default())
        }

        Method::ThreadDelete { thread_id } => {
            let deleted = server.thread_manager.delete_thread(&thread_id);
            RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
        }

        Method::TurnStart { thread_id, input } => {
            let agent_id = harness_core::AgentId::from_str(
                &server.config.agents.default_agent,
            );
            match server.thread_manager.start_turn(&thread_id, input, agent_id) {
                Ok(turn_id) => {
                    RpcResponse::success(id, serde_json::json!({ "turn_id": turn_id }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::TurnCancel { turn_id: _ } => {
            // Need thread_id to cancel — simplified for now
            RpcResponse::error(id, INTERNAL_ERROR, "turn cancel requires thread context")
        }

        Method::TurnStatus { turn_id: _ } => {
            RpcResponse::error(id, INTERNAL_ERROR, "turn status requires thread context")
        }

        // Placeholder for unimplemented methods
        _ => RpcResponse::error(id, METHOD_NOT_FOUND, "method not yet implemented"),
    }
}
