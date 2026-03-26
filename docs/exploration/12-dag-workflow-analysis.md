# 12 — DAG-based Workflow Engine for Harness

> Status: Exploration
> Author: Architecture Review
> Date: 2026-03-24

## 1. Current Limitations

### 1.1 No Task Dependencies

The `POST /tasks` endpoint spawns a task immediately. There is no way to express "start task B after task A completes." The only ordering mechanism is the `TaskQueue` semaphore, which enforces concurrency limits but not dependency order — if slots are available, tasks run in parallel regardless of logical relationships.

The consequence: users who need sequential execution (e.g., "implement feature X, then update docs for X") must use an external shell script:

```bash
# Current workaround: serial task submission
TASK_A=$(curl -s -X POST localhost:9800/tasks -d '{"issue": 42}' | jq -r '.id')
while [ "$(curl -s localhost:9800/tasks/$TASK_A | jq -r '.status')" != "done" ]; do
    sleep 30
done
curl -s -X POST localhost:9800/tasks -d '{"prompt": "Update docs for #42"}'
```

This is fragile, unrecoverable on crash, and invisible to the Harness dashboard.

### 1.2 No Workflow Composition

A sprint plan (triage N issues → implement → review → merge) is a workflow, but Harness models it as N independent tasks. There is no shared context between them, no conditional branching (skip low-priority issues), and no fan-out/fan-in coordination.

### 1.3 Limited Parallel Dispatch

`parallel_dispatch.rs` decomposes prompts by file references into at most 2 subtasks. This is hardcoded:

```rust
let mid = files.len().div_ceil(2);
```

There is no general fan-out mechanism. You cannot say "run these 5 issues in parallel, then run a synthesis step when all complete."

### 1.4 Review Loop is Internal to One Task

The review loop (`Implement → Review → LGTM/FIXED/WAITING`) is embedded in `task_executor.rs`. It cannot span multiple tasks — you cannot have one task implement and a separate task review, with the review verdict feeding back to the implementer. Cross-task review coordination does not exist.

### 1.5 No Task-Level Retry or Backoff

The current retry mechanism (`retry_base_backoff_ms`, `retry_max_backoff_ms`) applies only to validation retries within a single agent turn. If an entire task fails (agent crash, API timeout), there is no automatic retry. The user must manually re-submit.

---

## 2. DAG Workflow Proposal

### 2.1 YAML DSL

Inspired by Argo Workflows, Temporal, and Prefect, but simplified for Harness's domain (code tasks, not arbitrary containers):

```yaml
name: sprint-plan
version: 1
params:
  issues: [42, 43, 44, 45]
  repo: "owner/repo"
  max_rounds: 3

nodes:
  triage:
    type: fan_out
    items: "{{ params.issues }}"
    template: triage-issue
    max_parallel: 4

  triage-issue:
    type: task
    params:
      issue: "{{ item }}"
      agent: claude
      phase: triage
    on_success:
      - condition: "{{ output.decision == 'SKIP' }}"
        goto: done
      - condition: "{{ output.decision == 'PROCEED_WITH_PLAN' }}"
        goto: plan-issue
      - default:
        goto: implement-issue

  plan-issue:
    type: task
    params:
      issue: "{{ item }}"
      agent: claude
      phase: plan
    on_success:
      - goto: implement-issue

  implement-issue:
    type: task
    params:
      issue: "{{ item }}"
      agent: claude
      phase: implement
      max_rounds: "{{ params.max_rounds }}"
    on_success:
      - goto: review-issue
    on_failure:
      - goto: failed
    retry:
      max_attempts: 2
      backoff: exponential
      base_delay: 60s

  review-issue:
    type: task
    params:
      issue: "{{ item }}"
      agent: claude
      phase: review
    on_success:
      - condition: "{{ output.verdict == 'LGTM' }}"
        goto: merge-issue
      - condition: "{{ output.verdict == 'FIXED' }}"
        goto: implement-issue
      - condition: "{{ output.verdict == 'WAITING' }}"
        goto: waiting

  merge-issue:
    type: task
    params:
      prompt: "Merge the PR for issue #{{ item }}"
      agent: claude
    on_success:
      - goto: done

  waiting:
    type: delay
    duration: "{{ params.wait_secs | default(120) }}s"
    on_complete:
      - goto: review-issue

  done:
    type: terminal
    status: completed

  failed:
    type: terminal
    status: failed

edges:
  - from: triage
    to: triage-issue

conflict_groups:
  - name: review-serialization
    nodes: [review-issue, merge-issue]
    strategy: serial
```

### 2.2 WorkflowNode Enum

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowNode {
    /// Execute a Harness task (agent invocation).
    Task {
        params: TaskNodeParams,
        on_success: Vec<Transition>,
        on_failure: Vec<Transition>,
        timeout: Option<Duration>,
        retry: Option<RetryPolicy>,
    },

    /// Fan-out: instantiate a template for each item in a list.
    FanOut {
        items: Expression,
        template: String,
        max_parallel: Option<usize>,
    },

    /// Fan-in: wait for all items from a fan-out to complete.
    FanIn {
        fan_out_node: String,
        on_all_success: Vec<Transition>,
        on_any_failure: Vec<Transition>,
    },

    /// Delay: wait for a duration before proceeding.
    Delay {
        duration: Expression,
        on_complete: Vec<Transition>,
    },

    /// Conditional: evaluate an expression and branch.
    Conditional {
        branches: Vec<ConditionalBranch>,
    },

    /// Retry wrapper: re-execute a node on failure.
    Retry {
        target: String,
        policy: RetryPolicy,
    },

    /// Terminal: workflow end state.
    Terminal {
        status: TerminalStatus,
    },
}
```

### 2.3 TaskNodeParams

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNodeParams {
    /// Issue number, PR number, or free-text prompt.
    pub issue: Option<Expression>,
    pub pr: Option<Expression>,
    pub prompt: Option<Expression>,

    /// Agent to use (defaults to project default).
    pub agent: Option<String>,

    /// Pipeline phase to execute.
    pub phase: Option<TaskPhase>,

    /// Max review rounds (for implement+review loops).
    pub max_rounds: Option<Expression>,

    /// Additional context passed to the agent.
    pub context: Vec<ContextItem>,
}
```

### 2.4 WorkflowScheduler

```rust
pub struct WorkflowScheduler {
    workflow: WorkflowDef,
    state: WorkflowState,
    task_store: Arc<TaskOrchestrator>,  // or TaskStore in current arch
    db: WorkflowDb,
}

impl WorkflowScheduler {
    pub async fn run(&mut self) -> anyhow::Result<WorkflowResult> {
        loop {
            let current = &self.state.current_nodes;

            if current.is_empty() {
                return Ok(WorkflowResult::Completed);
            }

            // Execute all current nodes in parallel
            let mut handles = Vec::new();
            for node_id in current.clone() {
                let node = self.workflow.get_node(&node_id)?;
                let handle = self.execute_node(node_id, node).await?;
                handles.push(handle);
            }

            // Wait for any node to complete
            let (completed_id, outcome) = self.wait_any(&mut handles).await?;

            // Evaluate transitions
            let next_nodes = self.evaluate_transitions(&completed_id, &outcome)?;

            // Update state
            self.state.current_nodes.retain(|n| n != &completed_id);
            self.state.current_nodes.extend(next_nodes);
            self.state.completed_nodes.insert(completed_id, outcome);

            // Checkpoint
            self.checkpoint().await?;
        }
    }

    async fn checkpoint(&self) -> anyhow::Result<()> {
        let cp = WorkflowCheckpoint {
            workflow_id: self.state.workflow_id.clone(),
            current_nodes: self.state.current_nodes.clone(),
            completed_nodes: self.state.completed_nodes.clone(),
            params: self.state.resolved_params.clone(),
            timestamp: chrono::Utc::now(),
        };
        self.db.save_checkpoint(&cp).await
    }
}
```

### 2.5 Checkpointing and Recovery

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCheckpoint {
    pub workflow_id: String,
    pub current_nodes: Vec<String>,
    pub completed_nodes: HashMap<String, NodeOutcome>,
    pub params: HashMap<String, serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}
```

Recovery on restart:
1. Query `workflow_checkpoints` table for incomplete workflows (no terminal checkpoint).
2. For each, load the latest checkpoint.
3. Reconstruct `WorkflowState` from the checkpoint.
4. Resume `run()` from `current_nodes`.

Nodes that were in-flight at crash time are re-executed. This is safe because:
- Task nodes are idempotent (the agent creates or updates a PR; re-running produces the same result).
- Delay nodes are re-started (the timer resets; worst case, we wait an extra `duration`).
- Fan-out nodes check which children already completed (via `completed_nodes`) and only spawn missing ones.

---

## 3. Concrete Example: Sprint Plan Workflow

### 3.1 Execution Timeline

Given `issues: [42, 43, 44, 45]` with `max_parallel: 2`:

```
Time    Issue #42          Issue #43          Issue #44          Issue #45
─────   ───────────────    ───────────────    ───────────────    ───────────────
T+0     triage             triage             (queued)           (queued)
T+2m    → PROCEED          → SKIP (done)      triage             triage
T+3m    implement                             → PROCEED_W_PLAN   → PROCEED
T+4m    │                                     plan               implement
T+15m   implement done                        plan done          │
T+16m   review                                implement          │
T+18m   → LGTM                                │                  implement done
T+19m   merge                                 │                  review
T+20m   ✅ done                                implement done     → FIXED
T+21m                                         review             implement (retry)
T+23m                                         → LGTM             │
T+24m                                         merge              implement done
T+25m                                         ✅ done             review
T+27m                                                            → LGTM
T+28m                                                            merge
T+29m                                                            ✅ done
```

Key observations:
- Issue #43 was triaged as SKIP — no further work.
- Issue #44 required a plan phase (PROCEED_WITH_PLAN).
- Issue #45 received FIXED on first review — the implement phase re-ran with the reviewer's feedback.
- The `max_parallel: 2` limit means issues #44 and #45 start triage only after #42 and #43 finish theirs.

### 3.2 Conflict Group Serialization

The `conflict_groups` section ensures reviews and merges are serialized:

```yaml
conflict_groups:
  - name: review-serialization
    nodes: [review-issue, merge-issue]
    strategy: serial
```

This prevents two reviews from running simultaneously on the same branch (which would cause git conflicts). Implementation: the scheduler acquires a per-group semaphore (capacity 1) before executing nodes in the group.

### 3.3 Visualization

The workflow can be visualized as a DAG:

```
           ┌──────────┐
           │  triage   │ (fan-out over issues)
           └────┬──────┘
                │
        ┌───────┼────────┐
        ▼       ▼        ▼
     [SKIP]  [PROCEED] [PROCEED_W_PLAN]
        │       │        │
        ▼       │        ▼
      done      │    ┌────────┐
                │    │  plan   │
                │    └────┬───┘
                ▼         ▼
           ┌──────────────┐
           │  implement    │◄────────┐
           └──────┬───────┘         │
                  │                 │
                  ▼                 │
           ┌──────────────┐        │
           │   review      │───────┘ (FIXED)
           └──────┬───────┘
                  │
           ┌──────┴──────┐
           ▼              ▼
        [LGTM]        [WAITING]
           │              │
           ▼              ▼
       ┌────────┐    ┌─────────┐
       │ merge   │    │  delay  │──→ review
       └───┬────┘    └─────────┘
           ▼
         done
```

---

## 4. What It Solves

### 4.1 Task Ordering via Explicit DAG Edges
Dependencies are declared in YAML, not implied by shell script polling. The scheduler guarantees execution order. The user sees the full workflow graph in the dashboard.

### 4.2 Parallel-When-Safe with Fan-Out/Fan-In
Issues are processed in parallel (up to `max_parallel`), but reviews are serialized via conflict groups. This is the optimal strategy: maximize throughput while preventing git conflicts.

### 4.3 Conditional Branching Based on Outcomes
Triage decisions (`SKIP`, `PROCEED`, `PROCEED_WITH_PLAN`) route to different subgraphs. Review verdicts (`LGTM`, `FIXED`, `WAITING`) create loops. These are first-class constructs in the DAG, not if/else branches buried in `task_executor.rs`.

### 4.4 Crash Recovery via Checkpoint-Based Replay
Every node completion triggers a checkpoint. On restart, the scheduler resumes from the last checkpoint. In-flight nodes are re-executed (idempotent). Completed nodes are skipped. The user never needs to re-submit a partially completed sprint plan.

### 4.5 Workflow Templates
YAML workflows are reusable and composable. A team can define a `sprint-plan.yaml` template, parameterize it with issue lists, and submit it via `POST /workflows`. Different repos can use different templates.

---

## 5. What It Costs

### 5.1 Workflow Definition Complexity
Users must learn the YAML DSL. Simple tasks ("fix this bug") should not require a workflow definition — they should continue to use `POST /tasks` directly.

**Mitigation**: Workflows are opt-in. `POST /tasks` remains unchanged. `POST /workflows` is a new endpoint for multi-step orchestration. A `POST /workflows/from-sprint-plan` convenience endpoint could accept just an issue list and use a built-in template.

### 5.2 State Persistence
Each checkpoint is ~1-2KB (node IDs + outcome summaries). A 20-issue sprint plan generates ~100 checkpoints — 200KB total. SQLite handles this trivially.

### 5.3 Scheduler Code Complexity
The `WorkflowScheduler` is estimated at 1000-1500 lines:
- `scheduler.rs`: ~400 lines (main loop, node execution, transition evaluation)
- `workflow_def.rs`: ~200 lines (YAML parsing, validation)
- `dag.rs`: ~150 lines (topological sort, cycle detection via petgraph)
- `checkpoint.rs`: ~150 lines (persistence, recovery)
- `interpolation.rs`: ~200 lines (expression evaluation: `{{ params.issues }}`, `{{ output.verdict }}`)
- `conditions.rs`: ~100 lines (conditional branch evaluation)

### 5.4 Debugging Distributed Workflows
When a workflow gets stuck, the user must understand which node is blocked and why. This requires:
- A `GET /workflows/{id}` endpoint returning the current DAG state with per-node status.
- A `GET /workflows/{id}/events` endpoint returning the event log.
- Dashboard integration showing the DAG visualization.

### 5.5 Over-Engineering Risk
Simple tasks should not require a workflow. If the YAML DSL becomes the default interface, we have over-engineered.

**Mitigation**: Workflows are strictly opt-in. The `POST /tasks` endpoint remains the primary interface. Workflows are for multi-step orchestration only.

---

## 6. Rust Ecosystem

### 6.1 No Existing Fit

There is no Rust workflow engine that fits Harness's requirements:
- **Temporal** (temporal.io): Requires a separate server cluster. Overkill for single-machine orchestration.
- **Conductor** (Netflix): Java-based. No Rust client.
- **Prefect** (Python): Python-only.
- **Argo Workflows**: Kubernetes-native. Requires k8s.

All existing options are either in a different language, require external infrastructure, or are designed for container orchestration rather than code agent orchestration.

### 6.2 Build Custom with petgraph

The recommended approach: build a custom workflow engine using `petgraph` for DAG operations and `serde_yaml` for parsing.

**Dependencies**:
- `petgraph 0.6`: DAG data structure, topological sort, cycle detection. Mature, well-maintained, ~50KB.
- `serde_yaml 0.9`: YAML parsing/serialization. Already a transitive dependency via Harness configs.
- `tokio`: Already in use. Provides `select!`, channels, and timers for the scheduler.

### 6.3 New Crate Structure

```
crates/harness-workflow/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── workflow_def.rs    # WorkflowDef, YAML parsing, validation
│   ├── dag.rs             # petgraph integration, topo sort, cycle check
│   ├── scheduler.rs       # WorkflowScheduler, main loop
│   ├── checkpoint.rs      # WorkflowCheckpoint, DB persistence
│   ├── interpolation.rs   # Expression evaluation ({{ params.x }})
│   ├── conditions.rs      # Conditional branch evaluation
│   └── tests/
│       ├── sprint_plan.rs
│       └── fixtures/
│           └── sprint-plan.yaml
```

The crate depends on `harness-core` (for `TaskPhase`, `TaskId`, `ContextItem`) and `harness-server` (for `TaskStore` or `TaskOrchestrator`). It is consumed by `harness-server` to register the `/workflows` endpoint.

### 6.4 Implementation Roadmap

| Phase | Scope | Duration |
|---|---|---|
| P0: YAML + DAG | `workflow_def.rs`, `dag.rs` — parse YAML, build petgraph, validate (no cycles, all refs valid) | Week 1 |
| P1: Scheduler core | `scheduler.rs` — main loop, Task node execution, simple linear workflows | Week 2 |
| P2: Branching + Loops | Conditional transitions, FIXED→implement retry loop, WAITING→delay→review | Week 3 |
| P3: Fan-out/Fan-in | Template instantiation, parallel execution, conflict groups | Week 4-5 |
| P4: Checkpointing | `checkpoint.rs` — persistence, recovery, idempotent re-execution | Week 6 |
| P5: HTTP API + Dashboard | `POST /workflows`, `GET /workflows/{id}`, dashboard integration | Week 7 |

**Total: ~7 weeks.**

---

## 7. Migration Path

### 7.1 Backward Compatibility

The existing `POST /tasks` API is unchanged. Tasks continue to work exactly as they do today — single-step, immediate execution, review loop internal to the task.

Workflows are a **new** API surface:

```
POST   /workflows              # Submit a workflow (YAML body or JSON)
GET    /workflows              # List active workflows
GET    /workflows/{id}         # Get workflow state + DAG
DELETE /workflows/{id}         # Cancel a workflow
GET    /workflows/{id}/events  # Event log
```

### 7.2 Sprint Plan Migration

The current sprint plan flow (from `sprint_planner` intake):
1. Parse sprint plan YAML (issue list + priorities).
2. For each issue, create a task via `POST /tasks`.
3. Tasks run independently, no coordination.

With workflows:
1. Parse sprint plan YAML.
2. Generate a workflow YAML from the sprint plan (template instantiation).
3. Submit via `POST /workflows`.
4. The scheduler handles ordering, branching, and retry.

The sprint plan intake module becomes a thin adapter: it parses the plan format and emits a workflow definition.

### 7.3 Shell Script Migration

Users currently using external scripts for serial execution can replace them with a simple workflow:

```yaml
name: serial-tasks
nodes:
  step-1:
    type: task
    params:
      issue: 42
    on_success:
      - goto: step-2
  step-2:
    type: task
    params:
      prompt: "Update docs for #42"
    on_success:
      - goto: done
  done:
    type: terminal
    status: completed
```

---

## 8. Design Decisions

### 8.1 Why YAML and Not a Rust DSL?

YAML is:
- **External**: Users can define workflows without recompiling Harness.
- **Versionable**: Workflow definitions can be checked into the repo alongside the code.
- **Inspectable**: Non-engineers (PMs, tech leads) can read and modify workflows.

A Rust DSL would provide compile-time safety but require recompilation for every workflow change. Since workflows are user-defined and vary per project, YAML is the right choice.

### 8.2 Why petgraph and Not Adjacency Lists?

petgraph provides:
- `toposort()` for execution ordering.
- `is_cyclic_directed()` for validation.
- `Bfs`/`Dfs` iterators for visualization.
- `GraphMap` for efficient node lookup by name.

Rolling our own DAG would duplicate well-tested graph algorithms for no benefit.

### 8.3 Why Checkpoint-Based Recovery and Not Event Sourcing?

Event sourcing (replay all events from the beginning) is more general but:
- Requires a complete, ordered event log.
- Replay can be slow for long workflows (100+ nodes).
- Increases storage requirements.

Checkpoint-based recovery snapshots the workflow state after each node completion. Recovery reads one row from SQLite and resumes. This is simpler, faster, and sufficient for Harness's workflow sizes (typically 5-50 nodes).

### 8.4 Why Not Use the Actor Model (Doc 11) as the Foundation?

The DAG workflow engine and the actor model are complementary, not alternatives:
- **Actors** solve the state management problem at the task level (TaskActor owns TaskState).
- **Workflows** solve the coordination problem at the multi-task level (Scheduler orchestrates multiple tasks).

If both are implemented, the `WorkflowScheduler` sends commands to the `TaskOrchestrator`, which dispatches to `TaskActor` instances. The actor model is the execution substrate; the workflow engine is the coordination layer on top.

---

## 9. Open Questions

1. **Expression language**: What subset of expressions do we support? `{{ params.x }}` is simple string interpolation. Do we need arithmetic (`{{ params.count + 1 }}`), list operations (`{{ params.issues | length }}`), or is plain interpolation sufficient?
2. **Workflow versioning**: When a running workflow's YAML is updated, does the running instance continue with the old version or migrate? (Recommendation: continue with old version; new submissions use new version.)
3. **Cross-workflow dependencies**: Can one workflow depend on another? (Recommendation: no, keep workflows independent. Use a meta-workflow if needed.)
4. **Timeout semantics**: Does a node timeout cancel the underlying task, or just move to the `on_failure` transition while the task continues running? (Recommendation: cancel the task.)
5. **Dashboard visualization**: Should the DAG be rendered server-side (SVG) or client-side (D3.js)? (Recommendation: return the DAG as JSON; let the dashboard frontend render it.)
