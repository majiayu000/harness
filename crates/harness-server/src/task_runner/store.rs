use crate::task_db::TaskDb;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::StreamExt;
use harness_core::agent::StreamItem;
use harness_core::db::PgStoreContext;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex, RwLock};

use super::metrics::{DashboardCounts, LlmMetricsInputs, ProjectCounts};
use super::state::{SchedulerAuthorityState, SchedulerOwnerKind, TaskState, TaskSummary};
use super::types::{TaskId, TaskKind, TaskStatus, TaskTerminalOutcome};
use super::CompletionCallback;

mod persistence;
mod projections;
mod queries;
mod recovered_prs;
mod runtime_control;
mod runtime_hosts;
mod startup;
mod transitions;

/// Broadcast channel capacity for per-task stream events.
/// When the buffer is full, the oldest events are dropped for lagging receivers.
const TASK_STREAM_CAPACITY: usize = 512;

fn record_task_runner_usage() {
    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskRunner,
    );
}

pub struct TaskStore {
    pub(crate) cache: DashMap<TaskId, TaskState>,
    pub(super) db: TaskDb,
    persist_locks: DashMap<TaskId, Arc<Mutex<()>>>,
    /// Per-task broadcast channels for real-time stream forwarding to SSE clients.
    stream_txs: DashMap<TaskId, broadcast::Sender<StreamItem>>,
    /// Per-task abort handles for cooperative cancellation via Tokio task abort.
    abort_handles: DashMap<TaskId, tokio::task::AbortHandle>,
    /// Global circuit breaker: when the CLI account-level limit is hit,
    /// all tasks pause until this instant passes.
    rate_limit_until: RwLock<Option<tokio::time::Instant>>,
    /// Append-only JSONL event log for crash recovery. `None` if the file
    /// could not be opened (best-effort; server still starts without it).
    pub(crate) event_log: Option<Arc<crate::event_replay::TaskEventLog>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskSummaryFilter {
    pub statuses: Vec<TaskStatus>,
    pub scheduler_states: Vec<SchedulerAuthorityState>,
    pub kinds: Vec<TaskKind>,
    pub source: Option<String>,
    pub repo: Option<String>,
    pub project: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummaryPageCursor {
    pub created_at: DateTime<Utc>,
    pub id: String,
}

pub(super) use transitions::{mark_terminal_once, mutate_and_persist, update_status};

#[cfg(test)]
use recovered_prs::{recovered_pr_candidate, RecoveredPrCandidate};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalTransition {
    Applied,
    AlreadyTerminal(TaskStatus),
    MissingTask,
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
