//! JSON-RPC request router — **agent-facing (data plane) only**.
//!
//! This module handles the JSON-RPC 2.0 surface used by agents running inside
//! Harness threads (stdio, WebSocket, or HTTP `/rpc`).  It deliberately does
//! **not** handle task submission or project registration; those operations are
//! part of the operator-facing control plane and are exclusively served by the
//! HTTP REST routes defined in `http.rs`.
//!
//! See `docs/api-contract.md` for the full transport role description.

use crate::handlers;
use crate::http::AppState;
use harness_protocol::{Method, RpcRequest, RpcResponse};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(state: &AppState, req: RpcRequest) -> Option<RpcResponse> {
    use std::sync::atomic::Ordering;

    let id = req.id.clone();

    // Handshake gate: only Initialize and Initialized are allowed before handshake.
    match &req.method {
        Method::Initialize | Method::Initialized => {}
        _ if !state.notifications.initialized.load(Ordering::Relaxed) => {
            return Some(RpcResponse::error(
                id,
                harness_protocol::NOT_INITIALIZED,
                "Server not initialized. Send 'initialize' first.",
            ));
        }
        _ => {}
    }

    match req.method {
        // === Initialization ===
        Method::Initialize => {
            if state.notifications.initialized.load(Ordering::Relaxed) {
                return Some(RpcResponse::error(
                    id,
                    harness_protocol::INVALID_REQUEST,
                    "Server already initialized.",
                ));
            }
            state
                .notifications
                .initializing
                .store(true, Ordering::Relaxed);
            Some(handlers::thread::initialize(id).await)
        }
        Method::Initialized => {
            if !state.notifications.initializing.load(Ordering::Relaxed) {
                return Some(RpcResponse::error(
                    id,
                    harness_protocol::INVALID_REQUEST,
                    "Send 'initialize' before 'initialized'.",
                ));
            }
            state
                .notifications
                .initialized
                .store(true, Ordering::Relaxed);
            handlers::thread::initialized().await;
            if id.is_none() {
                None
            } else {
                Some(RpcResponse::success(id, serde_json::json!({})))
            }
        }

        // === Thread management ===
        Method::ThreadStart { cwd } => Some(handlers::thread::thread_start(state, id, cwd).await),
        Method::ThreadList => Some(handlers::thread::thread_list(state, id).await),
        Method::ThreadDelete { thread_id } => {
            Some(handlers::thread::thread_delete(state, id, thread_id).await)
        }
        Method::ThreadResume { thread_id } => {
            Some(handlers::thread::thread_resume(state, id, thread_id).await)
        }
        Method::ThreadFork {
            thread_id,
            from_turn,
        } => Some(handlers::thread::thread_fork(state, id, thread_id, from_turn).await),
        Method::ThreadCompact { thread_id } => {
            Some(handlers::thread::thread_compact(state, id, thread_id).await)
        }

        // === Turn control ===
        Method::TurnStart { thread_id, input } => {
            Some(handlers::thread::turn_start(state, id, thread_id, input).await)
        }
        Method::TurnCancel { turn_id } => {
            Some(handlers::thread::turn_cancel(state, id, turn_id).await)
        }
        Method::TurnStatus { turn_id } => {
            Some(handlers::thread::turn_status(state, id, turn_id).await)
        }
        Method::TurnSteer {
            turn_id,
            instruction,
        } => Some(handlers::thread::turn_steer(state, id, turn_id, instruction).await),

        // === Skills ===
        Method::SkillCreate { name, content } => {
            Some(handlers::skills::skill_create(state, id, name, content).await)
        }
        Method::SkillList { query } => Some(handlers::skills::skill_list(state, id, query).await),
        Method::SkillGet { skill_id } => {
            Some(handlers::skills::skill_get(state, id, skill_id).await)
        }
        Method::SkillDelete { skill_id } => {
            Some(handlers::skills::skill_delete(state, id, skill_id).await)
        }

        // === Events / Metrics ===
        Method::EventLog { event } => Some(handlers::observe::event_log(state, id, event).await),
        Method::EventQuery { filters } => {
            Some(handlers::observe::event_query(state, id, filters).await)
        }
        Method::MetricsCollect { project_root } => {
            Some(handlers::observe::metrics_collect(state, id, project_root).await)
        }
        Method::MetricsQuery { filters } => {
            Some(handlers::observe::metrics_query(state, id, filters).await)
        }

        // === Rules ===
        Method::RuleLoad { project_root } => {
            Some(handlers::rules::rule_load(state, id, project_root).await)
        }
        Method::RuleCheck {
            project_root,
            files,
        } => Some(handlers::rules::rule_check(state, id, project_root, files).await),

        // === GC ===
        Method::GcRun { project_id: _ } => Some(handlers::gc::gc_run(state, id).await),
        Method::GcStatus => Some(handlers::gc::gc_status(state, id).await),
        Method::GcDrafts { project_id: _ } => Some(handlers::gc::gc_drafts(state, id).await),
        Method::GcAdopt { draft_id } => Some(handlers::gc::gc_adopt(state, id, draft_id).await),
        Method::GcReject { draft_id, reason } => {
            Some(handlers::gc::gc_reject(state, id, draft_id, reason).await)
        }

        // === ExecPlan ===
        Method::ExecPlanInit { spec, project_root } => {
            Some(handlers::exec::exec_plan_init(state, id, spec, project_root).await)
        }
        Method::ExecPlanStatus { plan_id } => {
            Some(handlers::exec::exec_plan_status(state, id, plan_id).await)
        }
        Method::ExecPlanUpdate { plan_id, updates } => {
            Some(handlers::exec::exec_plan_update(state, id, plan_id, updates).await)
        }

        // === Task classification ===
        Method::TaskClassify { prompt, issue, pr } => {
            Some(handlers::classify::task_classify(id, prompt, issue, pr).await)
        }

        // === Learn feedback loop ===
        Method::LearnRules { project_root } => {
            Some(handlers::learn::learn_rules(state, id, project_root).await)
        }
        Method::LearnSkills { project_root } => {
            Some(handlers::learn::learn_skills(state, id, project_root).await)
        }

        // === Health & Stats ===
        Method::HealthCheck { project_root } => {
            Some(handlers::health::health_check(state, id, project_root).await)
        }
        Method::StatsQuery { since, until } => {
            Some(handlers::health::stats_query(state, id, since, until).await)
        }

        // === Agent management ===
        Method::AgentList => {
            let agents = state.core.server.agent_registry.list();
            Some(RpcResponse::success(
                id,
                serde_json::json!({ "agents": agents }),
            ))
        }

        // === VibeGuard ===
        Method::Preflight {
            project_root,
            task_description,
        } => Some(handlers::preflight::preflight(state, id, project_root, task_description).await),
        Method::CrossReview {
            project_root,
            target,
            max_rounds,
        } => Some(
            handlers::cross_review::cross_review(state, id, project_root, target, max_rounds).await,
        ),
    }
}

#[cfg(test)]
mod tests;
