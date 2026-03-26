# 11 — Event-Driven Actor Model for Harness

> Status: Exploration
> Author: Architecture Review
> Date: 2026-03-24

## 1. Current Architecture Pain Points

### 1.1 Shared Mutable State via DashMap

The `TaskStore` in `task_runner.rs` is the central state hub for all tasks. It wraps three concurrent data structures:

```rust
pub struct TaskStore {
    cache: DashMap<TaskId, TaskState>,
    persist_locks: DashMap<TaskId, Arc<Mutex<()>>>,
    stream_txs: DashMap<TaskId, broadcast::Sender<StreamItem>>,
    // ...
}
```

Every task mutation flows through `mutate_and_persist`, a free function that acquires a DashMap entry, mutates the `TaskState` in-place via a closure, then persists to SQLite under a per-task Mutex:

```rust
pub async fn mutate_and_persist(
    store: &TaskStore,
    db: &Option<TaskDb>,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()>
```

There are approximately **32 `tokio::spawn` calls** across `task_executor.rs` (1529 lines) and `task_runner.rs` (1607 lines) that mutate the cache through this function. The problems:

- **Non-local mutation**: Any spawned task can reach into the shared DashMap and modify any task's state. There is no ownership boundary — a bug in one task's closure can corrupt another task's state if the wrong `TaskId` is captured.
- **Persist lock contention**: The `persist_locks` DashMap creates a per-task Mutex for SQLite serialization. Under parallel dispatch (2 subtasks + 1 parent), three actors contend on the same DB connection through different lock instances.
- **No ordering guarantees**: Two concurrent `mutate_and_persist` calls for the same task can interleave. The DashMap entry lock is held only during the closure; the subsequent persist is under a separate Mutex. This means the DB can reflect state A while the cache holds state B if two mutations race.

### 1.2 Watcher Pattern Complexity

Task execution uses a dual-spawn pattern for crash safety:

```
spawn_task():
  spawn(run_task(...))         // the actual work
  spawn(watcher(handle, ...))  // monitors the task handle
```

The watcher holds a `JoinHandle` to the task spawn and catches panics, updating task status to `Failed` if the inner task panics. This works, but introduces:

- **Implicit dependencies via closure capture**: Both the task and watcher closures capture `Arc<TaskStore>`, `Arc<TaskDb>`, and the broadcast sender. The compiler ensures lifetime safety, but the *logical* dependency graph is invisible — you must read both closures to understand what state each touches.
- **Completion callback race on restart**: The `CompletionCallback` is an `Arc<dyn Fn(TaskState) -> Pin<Box<dyn Future>>>` set by the intake layer. On server restart, tasks are restored from SQLite but the callback is not persisted. If a task completes before the intake layer re-registers the callback, the completion notification is lost silently.
- **Dual-spawn overhead**: Every task consumes two tokio tasks. For a parallel dispatch with 2 subtasks, that is 6 tokio tasks (3 workers + 3 watchers) plus the parent's own 2 — 8 total for what is logically one unit of work.

### 1.3 Review Loop State Machine Fragmentation

The review loop in `task_executor.rs` implements a state machine: `Implementing → AgentReview → (LGTM → Done | FIXED → Implementing | WAITING → Waiting)`. Each transition requires separate `mutate_and_persist` calls:

1. Update `status` to `Reviewing`
2. Push a `RoundResult` to `rounds`
3. Update `status` to `Done` (or back to `Implementing`)

These are three independent cache+DB writes with no transactional atomicity. If the process crashes between step 1 and step 3, the task is left in `Reviewing` with a partial round — an inconsistent state that the restart logic must detect and handle (currently it does not).

### 1.4 Parallel Dispatch Coordination

`parallel_dispatch.rs` decomposes a prompt into subtasks by file references, then joins them:

```rust
let sub_prompts = decompose(&prompt);
// spawn subtasks, collect JoinHandles
let results = futures::future::join_all(handles).await;
```

Problems:
- **Synchronous join point**: The parent task blocks on `join_all`, consuming a tokio task and a semaphore permit while waiting. If the global concurrency limit is 3 and a parallel dispatch spawns 2 subtasks, the parent + 2 children consume all 3 slots — deadlock if another task is queued.
- **Implicit parent-child tracking**: Subtask IDs are generated as `{parent_id}-p{i}`. The parent stores `subtask_ids: Vec<TaskId>` in its `TaskState`, but this is populated at runtime and not persisted (`#[serde(default)]`). After restart, the parent-child relationship is reconstructed via `TaskStore::list_children`, a linear scan.
- **No task dependency system**: There is no way to express "task B should start after task A completes" without external scripting.

### 1.5 Task Queue Semaphore

`TaskQueue` uses a double-permit model — each task must acquire both a project-level and a global semaphore permit:

```rust
pub struct TaskPermit {
    _global_permit: OwnedSemaphorePermit,
    _project_permit: OwnedSemaphorePermit,
}
```

This prevents one project from starving others, but:
- **Double-permit mental model**: Developers must reason about two semaphores simultaneously. The acquisition order (project first, then global) is critical to avoid deadlock, but it is not enforced by the type system.
- **No task priority**: All tasks are FIFO within the semaphore queue. An urgent hotfix and a routine refactor wait in the same line.
- **No task affinity**: A task that writes to `src/auth.rs` and another task that writes to `src/auth.rs` will happily run in parallel, causing merge conflicts. There is no mechanism to serialize tasks that touch overlapping files.

---

## 2. Actor Model Proposal

### 2.1 Three-Tier Hierarchy

```
                    ┌─────────────────────┐
                    │  TaskOrchestrator    │
                    │  (router + supervisor)│
                    └──────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                 ▼
    ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
    │  TaskActor   │ │  TaskActor   │ │  TaskActor   │
    │  (task-001)  │ │  (task-002)  │ │  (task-003)  │
    └──────┬───────┘ └──────────────┘ └──────────────┘
           │
    ┌──────┴───────┐
    │  Internal    │
    │  StateMachine│
    └──────────────┘
```

- **TaskOrchestrator**: Single actor, receives all external requests (`create`, `cancel`, `get_status`). Routes to per-task actors. Manages spawn/shutdown lifecycle. Maintains the global view (task list, queue stats).
- **TaskActor**: One per task. Owns `TaskState` exclusively — no shared references. Communicates via message passing only. Runs the phase pipeline (Triage → Plan → Implement → Review → Terminal) internally.
- **Internal State Machine**: Embedded in each `TaskActor`. Transitions are atomic — a phase change is a single message processed in the actor's `run()` loop.

### 2.2 TaskEvent Enum

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskEvent {
    // Lifecycle
    Created { id: TaskId, request: CreateTaskRequest },
    PhaseTransition { from: TaskPhase, to: TaskPhase },
    StatusChanged { from: TaskStatus, to: TaskStatus },

    // Agent interaction
    AgentStarted { agent: String, phase: TaskPhase },
    AgentCompleted { output: String, token_usage: TokenUsage },
    AgentFailed { error: String },

    // Review loop
    RoundCompleted { round: RoundResult },

    // Streaming
    StreamItemPublished { item: StreamItem },

    // Terminal
    TaskCompleted { pr_url: Option<String> },
    TaskFailed { error: String },

    // Parallel dispatch
    SubtaskSpawned { child_id: TaskId },
    SubtaskCompleted { child_id: TaskId, success: bool },
}
```

Events are append-only. The `TaskActor` processes commands (imperative) and emits events (facts). This enables:
- **Event replay**: On restart, replay events from the last checkpoint to reconstruct state.
- **Audit log**: Every state transition is recorded with a timestamp.
- **External subscribers**: The SSE endpoint subscribes to the event stream instead of polling DashMap.

### 2.3 TaskActor Implementation

```rust
struct TaskActor {
    id: TaskId,
    state: TaskState,
    mailbox: mpsc::Receiver<TaskCommand>,
    event_log: Vec<TaskEvent>,
    event_tx: broadcast::Sender<TaskEvent>,
    db: Option<TaskDb>,
    agent_registry: Arc<AgentRegistry>,
}

impl TaskActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(cmd) = self.mailbox.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                    self.check_stall().await;
                }
            }

            if self.state.phase == TaskPhase::Terminal {
                self.persist_final_state().await;
                break;
            }
        }
    }

    async fn handle_command(&mut self, cmd: TaskCommand) {
        match cmd {
            TaskCommand::StartPhase(phase) => {
                let from = self.state.phase.clone();
                self.state.phase = phase.clone();
                self.emit(TaskEvent::PhaseTransition { from, to: phase });
                self.run_phase().await;
            }
            TaskCommand::Cancel => {
                self.state.status = TaskStatus::Failed;
                self.state.error = Some("Cancelled by user".into());
                self.emit(TaskEvent::TaskFailed { error: "Cancelled".into() });
                self.state.phase = TaskPhase::Terminal;
            }
            TaskCommand::GetState(reply) => {
                let _ = reply.send(self.state.clone());
            }
        }
    }

    fn emit(&mut self, event: TaskEvent) {
        self.event_log.push(event.clone());
        let _ = self.event_tx.send(event);
    }
}
```

Key properties:
- **Single-threaded per task**: The `run()` loop processes one command at a time. No locks needed.
- **Heartbeat**: The `select!` includes a periodic check for agent stalls (no stream output within `stall_timeout_secs`).
- **Graceful shutdown**: When the phase reaches `Terminal`, the actor persists final state and exits.

### 2.4 Orchestrator Supervision

```rust
struct TaskOrchestrator {
    actors: HashMap<TaskId, (mpsc::Sender<TaskCommand>, JoinHandle<()>)>,
    queue: TaskQueue,
    db: Option<TaskDb>,
}

impl TaskOrchestrator {
    async fn spawn_task(&mut self, req: CreateTaskRequest) -> TaskId {
        let permit = self.queue.acquire(&req.project_id()).await?;
        let (tx, rx) = mpsc::channel(64);
        let actor = TaskActor::new(req, rx, self.db.clone());
        let handle = tokio::spawn(actor.run());
        self.actors.insert(id.clone(), (tx, handle));
        // Watcher for panic detection
        let id_clone = id.clone();
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                // Actor panicked — log and update DB directly
                tracing::error!(task = %id_clone, "actor panicked: {e}");
            }
            drop(permit); // Release queue slot
        });
        id
    }
}
```

The orchestrator replaces the current `TaskStore` + `spawn_task` + watcher pattern with a single supervision point. Panic recovery is still via a watcher, but the watcher only manages the permit lifecycle — it does not touch task state (the actor owns that exclusively).

### 2.5 Comparison Table

| Dimension | Current (DashMap + spawn) | Actor Model |
|---|---|---|
| **State ownership** | Shared via `DashMap<TaskId, TaskState>` — any spawn can mutate any task | Exclusive — each actor owns its `TaskState`, no external access |
| **Concurrency control** | `persist_locks` Mutex per task + DashMap entry locks | Message queue — one command at a time, no locks |
| **Error handling** | Dual-spawn watcher catches panics, updates shared state | Supervisor detects actor exit, can restart or mark failed |
| **Crash recovery** | Restore from SQLite on restart; in-flight state lost | Event replay from checkpoint; in-flight state recoverable |
| **State transitions** | 3 separate `mutate_and_persist` calls; no atomicity | Single message handler; atomic within the actor's `run()` loop |
| **Testability** | Must mock `TaskStore`, `TaskDb`, broadcast channels | Send commands to actor mailbox, assert emitted events |
| **Observability** | Poll DashMap for status; SSE re-broadcasts stream items | Subscribe to `TaskEvent` stream; full audit log |
| **Code locality** | State mutations scattered across 32+ spawn sites in 2 files totaling 3136 lines | State mutations centralized in `TaskActor::handle_command` |

---

## 3. What It Solves

### 3.1 Serial vs Parallel Execution
Actors are naturally concurrent. Each `TaskActor` runs independently. The orchestrator controls concurrency via the queue — no need for the double-permit semaphore model. Parallel dispatch becomes: orchestrator spawns N child actors, each runs independently, parent actor receives `SubtaskCompleted` events.

### 3.2 Crash Recovery
The current system loses in-flight state on crash. With the actor model:
1. Each actor periodically checkpoints its event log to SQLite.
2. On restart, the orchestrator queries the DB for incomplete tasks.
3. For each, it creates a new `TaskActor`, replays events from the last checkpoint, and resumes from the reconstructed state.

This is analogous to event sourcing — the event log is the source of truth, and `TaskState` is a projection.

### 3.3 State Isolation
No more `DashMap` entry races. Each actor owns its state. The only way to read task state is to send a `GetState` command and receive a clone via a `oneshot` channel. This eliminates an entire class of concurrency bugs.

### 3.4 Backpressure
Queue permit is acquired before the actor is spawned. The permit is held by the watcher and released when the actor exits (success, failure, or panic). No more double-permit mental model — one permit per task, period.

### 3.5 Coordination
Parent-child relationships are explicit: the parent actor holds `mpsc::Sender` handles to child actors and receives completion events. No more linear scans or naming conventions (`{id}-p{i}`).

---

## 4. What It Costs

### 4.1 Complexity
The actor model introduces a new concurrency paradigm. Developers must reason about message ordering, mailbox backpressure, and supervision trees instead of locks and shared state. The learning curve is real — debugging a stuck actor requires understanding the message flow, not just inspecting shared state.

### 4.2 Message Serialization Overhead
Each `TaskCommand` and `TaskEvent` is cloned when sent through channels. For a typical task:
- ~20 events over its lifetime
- ~3KB per serialized `TaskEvent` (including `RoundResult` with reviewer output)
- Total: ~60KB per task — negligible compared to the agent's token usage (~$0.03 per task).

### 4.3 Migration Effort

| Phase | Scope | Estimate |
|---|---|---|
| P0: Define types | `TaskEvent`, `TaskCommand`, `TaskActor` struct | 2-3 days |
| P1: Actor core | `TaskActor::run()` loop, command handling, event emission | 4-5 days |
| P2: Orchestrator | Spawn, supervision, watcher, queue integration | 3-4 days |
| P3: Phase pipeline | Triage → Plan → Implement → Review migration | 4-5 days |
| P4: Parallel dispatch | Child actor spawning, `SubtaskCompleted` handling | 3-4 days |
| P5: Persistence | Event checkpointing, replay-based recovery | 3-4 days |
| P6: Migration | Replace `TaskStore` callsites, update HTTP handlers | 3-6 days |
| **Total** | | **22-31 days** |

### 4.4 Key Files Affected

| File | Lines | Role | Change |
|---|---|---|---|
| `task_runner.rs` | 1607 | TaskStore, TaskState, spawn_task | Replace with TaskOrchestrator |
| `task_executor.rs` | 1529 | run_task, review loop, phase pipeline | Move into TaskActor |
| `http.rs` | ~800 | AppState, axum routes | Replace `TaskStore` with orchestrator handle |
| `task_queue.rs` | ~200 | Semaphore-based queue | Simplify to single-permit model |
| `parallel_dispatch.rs` | ~200 | decompose + join | Replace with child actor spawning |
| `task_db.rs` | ~400 | SQLite persistence | Add event log table, checkpoint queries |

---

## 5. Framework Options

### 5.1 Custom tokio + mpsc (Recommended)

Build actors from `tokio::sync::mpsc` channels and `tokio::spawn`. This is what the current codebase already uses implicitly — the dual-spawn pattern is a proto-actor.

- **Pros**: Zero new dependencies. Full control over supervision, backpressure, and shutdown. Matches the team's existing tokio expertise.
- **Cons**: Must implement supervision, restart policies, and event replay manually.
- **Effort**: Included in the 22-31 day estimate above.

### 5.2 ractor (~80KB dependency)

A Rust actor framework built on tokio. Provides `Actor` trait, supervision trees, and named actor registry out of the box.

- **Pros**: Batteries-included supervision. Named actors simplify orchestrator lookups. Built-in `call` (request-response) and `cast` (fire-and-forget) patterns.
- **Cons**: Additional dependency. Framework opinions may conflict with Harness patterns (e.g., ractor's actor lifecycle vs. Harness's phase pipeline). Less control over internals.
- **Effort**: ~15-20 days (framework handles supervision boilerplate).

### 5.3 stakker (lightweight, niche)

A single-threaded actor runtime. Designed for low-latency scenarios.

- **Pros**: Very lightweight (~20KB). Deterministic execution (single-threaded).
- **Cons**: Single-threaded model does not fit Harness's multi-task concurrency. Small community. No async support — would require bridging with tokio.
- **Verdict**: Not suitable.

### 5.4 Recommendation

**Start with custom tokio + mpsc.** The migration is already large; adding a framework increases the surface area of unknowns. The custom approach lets us introduce actors incrementally — one phase at a time — without committing to a framework's lifecycle model upfront.

If the custom approach proves too verbose after P2 (Orchestrator), revisit ractor as a drop-in replacement for the supervision layer.

---

## 6. Migration Strategy

### Phase 0 — Types (Week 1)
Define `TaskEvent`, `TaskCommand`, `TaskActor` struct in a new `task_actor.rs`. No behavioral changes. Existing code continues to use `TaskStore`.

### Phase 1 — Actor Core (Week 1-2)
Implement `TaskActor::run()` with a simplified command set: `StartPhase(Implement)`, `Cancel`, `GetState`. Wire it up for new tasks only (feature flag). Existing tasks continue through the old path.

### Phase 2 — Orchestrator (Week 2-3)
Replace `TaskStore::spawn_task` with `TaskOrchestrator::spawn_task`. Migrate the watcher pattern. Validate with integration tests.

### Phase 3 — Phase Pipeline (Week 3-4)
Move Triage → Plan → Implement → Review logic from `task_executor.rs` into `TaskActor::run_phase()`. This is the largest phase — it touches the review loop state machine.

### Phase 4 — Parallel Dispatch (Week 5)
Replace `parallel_dispatch::decompose` + `join_all` with child actor spawning. Parent actor receives `SubtaskCompleted` events instead of blocking on `JoinHandle`.

### Phase 5 — Persistence (Week 5-6)
Add `task_events` table to SQLite. Implement checkpointing (every N events or on phase transition). Implement replay-based recovery on startup.

### Phase 6 — Cleanup (Week 6-7)
Remove `TaskStore`, `mutate_and_persist`, `persist_locks`, dual-spawn watcher. Update HTTP handlers to communicate with orchestrator. Remove feature flag.

---

## 7. Open Questions

1. **Event log retention**: How long do we keep events? Indefinitely (audit trail) or TTL-based (7 days)?
2. **Actor restart policy**: On panic, should the supervisor restart the actor from the last checkpoint, or mark the task as failed and let the user retry?
3. **Cross-actor queries**: The dashboard endpoint (`GET /tasks`) currently scans the DashMap. With actors, it must either query the orchestrator (which queries each actor) or maintain a read-only projection. Which?
4. **Subtask cancellation**: If a parent task is cancelled, should child actors be cancelled automatically? The current `join_all` model does not support this.
5. **Backpressure on event subscribers**: If the SSE client is slow, should the event broadcast drop old events (current behavior with `broadcast`) or buffer? Actors make this decision explicit.
