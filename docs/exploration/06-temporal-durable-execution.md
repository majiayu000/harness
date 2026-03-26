# Temporal and Durable Execution for AI Agents

## 1. How Temporal Works

Temporal is a durable execution platform that guarantees workflow completion even through failures, restarts, and infrastructure outages.

### Core Concepts

#### Workflows and Activities

```
Workflow (deterministic orchestration logic):
  ├── Activity 1: Call external API
  ├── Activity 2: Process data
  ├── Timer: Wait 5 minutes
  ├── Activity 3: Store results
  └── Activity 4: Send notification
```

- **Workflow**: deterministic function that orchestrates activities. Must not have side effects — same inputs always produce same outputs.
- **Activity**: non-deterministic function that does actual work (API calls, file I/O, LLM calls). Can fail and be retried.
- **Worker**: process that polls for and executes workflows and activities.

#### Event History and Replay

Temporal records every step as an event:

```
Event 1: WorkflowStarted
Event 2: ActivityScheduled(call_api)
Event 3: ActivityStarted(call_api)
Event 4: ActivityCompleted(call_api, result={...})
Event 5: ActivityScheduled(process_data)
...
```

On failure/restart, the workflow function is **replayed** against the event history:
- Events that already happened are skipped (results loaded from history)
- Execution continues from where it left off
- This is how durability works — no checkpointing needed

#### Deterministic Constraints

Workflow code must be deterministic. This means:

- **No random numbers** (use workflow-provided random)
- **No system time** (use workflow-provided time)
- **No external I/O** (all I/O must be in activities)
- **No mutable global state** (state must be in workflow scope)
- **Same code path on replay** (version workflows for code changes)

These constraints exist because the workflow function is re-executed during replay. Non-deterministic code would take different branches on replay, corrupting state.

### Durability Guarantee

```
Scenario: Worker crashes after Activity 2 completes

Without Temporal:
  Start → Activity 1 ✓ → Activity 2 ✓ → [CRASH] → Lost

With Temporal:
  Start → Activity 1 ✓ → Activity 2 ✓ → [CRASH]
  [Worker restarts]
  Replay → Activity 1 (skip, use stored result) → Activity 2 (skip, use stored result) → Activity 3 (execute) → ...
```

---

## 2. Temporal for AI Agents

### Agent Loop as Workflow

```python
@workflow.defn
class AgentWorkflow:
    @workflow.run
    async def run(self, task: Task) -> Result:
        context = await workflow.execute_activity(
            load_context, task, start_to_close_timeout=timedelta(seconds=30)
        )

        for turn in range(MAX_TURNS):
            # LLM call as activity (non-deterministic)
            response = await workflow.execute_activity(
                call_llm, context, start_to_close_timeout=timedelta(minutes=5)
            )

            if response.has_tool_calls:
                # Tool execution as activity
                results = await workflow.execute_activity(
                    execute_tools, response.tool_calls,
                    start_to_close_timeout=timedelta(minutes=10)
                )
                context.add_tool_results(results)
            else:
                return Result(output=response.text)

        return Result(output="Max turns reached", status="incomplete")
```

### Benefits for Agents

1. **Crash recovery**: agent resumes from last completed step, not from scratch
2. **Long-running tasks**: workflows can run for days/weeks without connection
3. **Tool execution isolation**: each tool call is an activity with independent timeout and retry
4. **Human-in-the-loop**: signals allow pausing for human approval

### Human-in-the-Loop via Signals

```python
@workflow.defn
class ReviewWorkflow:
    def __init__(self):
        self.approval = None

    @workflow.signal
    async def approve(self, approved: bool, feedback: str):
        self.approval = (approved, feedback)

    @workflow.run
    async def run(self, pr_url: str):
        review = await workflow.execute_activity(generate_review, pr_url)

        # Wait for human signal (can wait indefinitely)
        await workflow.wait_condition(lambda: self.approval is not None)

        approved, feedback = self.approval
        if approved:
            await workflow.execute_activity(merge_pr, pr_url)
        else:
            await workflow.execute_activity(address_feedback, pr_url, feedback)
```

---

## 3. Case Study: Replit Migration to Temporal

### Context

Replit migrated their AI agent system to Temporal in approximately 2 weeks.

### Architecture

```
┌────────────────────────────────────────┐
│              User Browser               │
│         (WebSocket connection)          │
└──────────────┬─────────────────────────┘
               │ Real-time streaming
               │
┌──────────────▼─────────────────────────┐
│           API Server                     │
│  ┌─────────────┐  ┌─────────────────┐  │
│  │  WebSocket   │  │ Temporal Client │  │
│  │  Handler     │  │                 │  │
│  └──────┬──────┘  └───────┬─────────┘  │
└─────────┼──────────────────┼───────────┘
          │                  │
    Streaming            Durability
    (tokens,             (state,
     status)              recovery)
          │                  │
┌─────────▼──────────────────▼───────────┐
│         Temporal Workflow                │
│  Agent Session = Workflow                │
│  ├── Tool Call = Activity                │
│  ├── LLM Call = Activity                 │
│  └── File Edit = Activity                │
└────────────────────────────────────────┘
```

### Dual-Channel Architecture

The key innovation: **Temporal for durability, WebSocket for streaming**.

- **Temporal channel**: handles state persistence, crash recovery, retry logic
- **WebSocket channel**: handles real-time token streaming to the user

Why dual-channel? Temporal doesn't support streaming natively. LLM responses need to stream token-by-token to the UI, but Temporal activities return only after completion. The WebSocket carries the real-time stream while Temporal ensures the overall workflow is durable.

### Lessons Learned

1. **Two weeks to migrate**: core infrastructure was straightforward, edge cases took time
2. **Deterministic constraint friction**: had to refactor agent logic to separate deterministic orchestration from non-deterministic execution
3. **Event history growth**: long agent sessions generate large event histories (thousands of events), causing slow replays
4. **Token streaming workaround**: required the dual-channel approach because Temporal activities are atomic

---

## 4. OpenAI Codex on Temporal

OpenAI has confirmed that Codex (their cloud coding agent) uses Temporal for workflow orchestration. Details are sparse:

- Each coding session is a Temporal workflow
- Tool execution (file read/write, terminal commands) are activities
- Enables session resumption after disconnection
- Supports the "background agent" mode where sessions run without active connection

No detailed architecture has been published. The confirmation came from job postings and brief mentions in engineering blog posts.

---

## 5. Temporal vs Alternatives

| Dimension | Temporal | Inngest | Hatchet | Windmill | Trigger.dev |
|-----------|----------|---------|---------|----------|-------------|
| **Recovery model** | Event replay | Checkpoint | Checkpoint | Checkpoint | Checkpoint |
| **Language support** | Go, Java, Python, TS, .NET | TS, Python, Go | Python, TS, Go | Python, TS, Go, Bash | TS, Python |
| **Self-hosted** | Yes (complex) | Yes | Yes | Yes (simple) | Yes |
| **Cloud offering** | Temporal Cloud | Inngest Cloud | Hatchet Cloud | Windmill Cloud | Trigger.dev Cloud |
| **Streaming** | No native | Realtime (built-in) | No native | Yes | Yes (realtime) |
| **Human-in-loop** | Signals | Wait-for-event | Wait-for-event | Approval steps | Wait-for-event |
| **Pricing** | Per-action | Per-step | Per-step | Per-execution | Per-run |
| **Complexity** | High | Medium | Medium | Low | Low |
| **Maturity** | Very High (2020) | Medium (2023) | Low (2024) | Medium (2022) | Medium (2023) |
| **AI agent focus** | General purpose | Growing | AI-native | General purpose | Growing |
| **Rust SDK** | No (sdk-core internal) | No | No | No | No |

### Notable Alternatives

**Inngest**: Event-driven, built-in streaming, simpler mental model than Temporal. Growing AI agent ecosystem. Steps are checkpointed, not replayed.

**Hatchet**: AI-native from the start. Designed for agent workflows with built-in concurrency controls. Newest entrant.

**Windmill**: Script-oriented, great for data pipelines and simple automations. Lower learning curve. Has a visual flow editor.

**Trigger.dev**: Developer-focused, good TypeScript DX. Realtime streaming built in. v3 has significant improvements for long-running tasks.

---

## 6. Replay-Based vs Checkpoint-Based: The Fundamental Split

### Replay-Based (Temporal)

```
Event history: [e1, e2, e3, e4, e5]
Recovery: replay e1→e2→e3→e4→e5 → continue from e5
```

**Pros**:
- Complete audit trail
- No serialization/deserialization of complex state
- Can replay to any point in history
- Workflow code is the source of truth

**Cons**:
- O(n) replay time where n = number of events
- Deterministic constraint is painful
- Event history can grow very large
- Code versioning is complex (what if workflow code changed?)

### Checkpoint-Based (Everyone Else)

```
Checkpoints: [C1, C2, C3]
Recovery: load C3 → continue from C3
```

**Pros**:
- O(1) recovery time — just load latest checkpoint
- No deterministic constraint
- Simpler mental model
- Smaller storage footprint (only latest state needed)

**Cons**:
- No audit trail (unless separately implemented)
- Serialization/deserialization can be complex for rich state
- Cannot replay to arbitrary historical point
- Checkpoint format is a coupling point

### For AI Agents

| Consideration | Replay (Temporal) | Checkpoint (Others) |
|---|---|---|
| **Long sessions (1000+ turns)** | Slow replay | Fast recovery |
| **Debugging** | Excellent — replay step by step | Limited — only checkpoint state |
| **Audit trail** | Built-in | Must add separately |
| **Streaming** | Not supported | Often built-in |
| **Operational complexity** | High | Lower |
| **Cold start** | Slow (replay all events) | Fast (load checkpoint) |

---

## 7. Limitations of Temporal for AI Agents

### Deterministic Constraint Friction

LLM calls are inherently non-deterministic. This means every LLM call must be an activity (not inline workflow code), which:
- Adds boilerplate for wrapping every model call
- Makes the workflow code less readable
- Prevents using LLM results in workflow control flow without intermediate serialization

### Event History Growth

A typical agent session:
- 20 turns × (1 LLM call + 3 tool calls) = 80 activities
- Each activity generates 3-4 events (scheduled, started, completed)
- 80 × 4 = 320 events per session

For long-running agents:
- 1000+ events → replay takes several seconds
- 10,000+ events → replay takes 30+ seconds
- Temporal recommends "continue-as-new" to truncate history, but this loses replay ability

### Cold Start Latency

When a workflow needs to resume:
1. Load event history from database
2. Instantiate workflow function
3. Replay all events (re-execute workflow code, skip completed activities)
4. Continue from where it left off

For a 500-event history, this takes 2-5 seconds. Not acceptable for interactive agent sessions.

### No Native Streaming

Temporal activities are atomic — they start and complete. There's no built-in way to stream partial results from an activity. For LLM token streaming, this requires the dual-channel workaround (see Replit section).

### Operational Complexity

Running Temporal requires:
- Temporal server (3 services: frontend, history, matching)
- Database (Cassandra, MySQL, or PostgreSQL)
- Elasticsearch (optional, for visibility)
- Worker processes

This is a significant operational burden for a small project.

---

## 8. No Rust SDK

Temporal has SDKs for Go, Java, Python, TypeScript, and .NET. There is no official Rust SDK.

### sdk-core

The `temporalio/sdk-core` repository exists — it's a Rust library that implements the core Temporal protocol. However:
- It's **internal** — used as the foundation for other SDKs (Python and TypeScript SDKs are built on sdk-core via FFI)
- Not intended for direct use by application developers
- No stable API, no documentation, no examples
- Breaking changes without notice

### Restate: Rust-Native Alternative

**Restate** (https://restate.dev) is the closest Rust-native alternative to Temporal:
- Written in Rust
- Has a Rust SDK
- Durable execution with replay
- Simpler operational model (single binary)
- Growing but small community

However, Restate has its own tradeoffs:
- Younger project (2023)
- Smaller ecosystem
- Different programming model (virtual objects vs workflows)
- Less battle-tested in production

---

## 9. Conclusion: Adopt the Patterns, Not the Platform

For Harness, the right approach is to adopt Temporal's **patterns** without taking a dependency on the Temporal **platform**:

### Patterns to Adopt

#### 1. Event Sourcing

```
Current Harness:
  events.jsonl → audit log (append-only, not used for state)

Temporal-Inspired:
  events.jsonl → source of truth
  task state → derived from events
  recovery → replay events to reconstruct state
```

#### 2. Activity/Workflow Split

```
Workflow (deterministic): task lifecycle management
  - State transitions
  - Review loop control
  - Task dependencies

Activities (non-deterministic): actual work
  - Agent execution
  - Tool calls
  - External API calls
```

#### 3. Signals for Human-in-the-Loop

```
Task: "Review PR #42"
Agent generates review → Signal: "waiting_for_human"
Human approves → Signal: "approved"
Agent merges → Task complete
```

#### 4. Timeout and Retry Policies

```
Agent execution: timeout 30 minutes, retry 0 (human decides)
Tool call: timeout 5 minutes, retry 3 with exponential backoff
Review: timeout 1 hour, retry 0
```

### Why Not the Platform

1. **No Rust SDK**: would require FFI or a sidecar process
2. **Operational complexity**: Temporal server is overkill for single-node Harness
3. **Deterministic constraint**: unnecessarily restrictive for Harness's use case
4. **SQLite is sufficient**: Harness's event log + SQLite provides adequate durability for single-node deployment

### Implementation Path

```
Phase 1: Formalize event sourcing
  - events.jsonl becomes source of truth
  - Task state derived from events
  - Add event types for all state transitions

Phase 2: Add activity abstraction
  - Separate orchestration logic from execution logic
  - Each activity has timeout and retry config
  - Activities are independently recoverable

Phase 3: Add signals
  - Human approval/rejection signals
  - Inter-task coordination signals
  - External webhook signals

Phase 4: Add durability
  - On crash: replay events to recover task state
  - On restart: resume in-progress tasks from last event
  - Periodic checkpointing for fast recovery (hybrid approach)
```

This gives Harness the durability guarantees of Temporal without the operational complexity, using SQLite as the event store and Rust's type system for the activity/workflow separation.
