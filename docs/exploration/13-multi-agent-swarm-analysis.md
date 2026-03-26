# 13 — Multi-Agent Swarm Architecture for Harness

> Status: Exploration
> Author: Architecture Review
> Date: 2026-03-24

## 1. Current Single-Agent Limitations

### 1.1 One Agent Does Everything

Today, a single Claude Code agent handles the entire task lifecycle: triage, planning, implementation, testing, and review. The `TaskPhase` enum (`Triage → Plan → Implement → Review → Terminal`) represents logical phases, but they all execute within the same agent context. The agent accumulates context across phases — triage reasoning bleeds into implementation, and implementation details pollute the review.

From `task_runner.rs`:
```rust
pub enum TaskPhase {
    Triage,
    Plan,
    Implement,
    Review,
    Terminal,
}
```

Each phase runs as a separate agent turn, but the agent carries forward its full session history. By the time the review phase starts, the agent has seen the issue, the triage assessment, the plan, and all implementation details — making it a biased reviewer of its own work.

### 1.2 Context Window Pollution

A complex task (e.g., "implement metrics endpoint with auth") generates:
- Triage: ~500 tokens (issue analysis)
- Plan: ~1200 tokens (architecture, file list)
- Implementation: ~3000 tokens (code reading, writing, testing)
- Review: ~800 tokens (diff analysis)

Total: ~5500 tokens of accumulated context. The review phase operates on the full 5500-token history, but only needs the diff (~800 tokens). The remaining 4700 tokens are noise that:
- Increases cost (tokens billed for context).
- Increases latency (longer prompt processing).
- Biases the review (the agent already "knows" why the code is correct — it wrote it).

### 1.3 No Parallelism Within a Task

The phase pipeline is strictly sequential:
```
Triage → Plan → Implement → Review → (loop or done)
```

But some phases are naturally parallelizable:
- Implementation and test writing can happen simultaneously on separate files.
- Multiple reviewers can review the same diff concurrently (diverse perspectives).
- A lint/style check can run while the semantic review is in progress.

The current architecture cannot express this — each phase must complete before the next begins.

### 1.4 Review is Sequential, Not Collaborative

The review loop (`task_executor.rs`) uses a single reviewer. If the reviewer says FIXED, the implementer re-runs. There is no second opinion, no escalation path, and no synthesis of multiple review perspectives.

The `periodic_reviewer.rs` is the closest thing to multi-agent review — it spawns Claude and Codex in parallel, then synthesizes their outputs. But this is a standalone periodic process, not integrated into the task pipeline.

### 1.5 No Model Specialization Per Role

All phases use the same model (configured per agent or via `ReasoningBudget`). But the cost-quality tradeoff differs by phase:
- **Triage**: Simple classification. Haiku-class model is sufficient (~$0.001/task).
- **Planning**: Moderate reasoning. Sonnet-class model (~$0.005/task).
- **Implementation**: Deep reasoning + code generation. Sonnet or Opus (~$0.02/task).
- **Testing**: Pattern matching + coverage analysis. Sonnet (~$0.005/task).
- **Review**: Code analysis + security check. Codex or Sonnet (~$0.005/task).

Using Opus for triage wastes 10x the cost for equivalent quality.

---

## 2. Swarm Proposal: 6 Specialized Agents

### 2.1 Agent Roles

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Routes phases to specialized agents. Manages lifecycle.
    Supervisor,
    /// Tech Lead: evaluates issue complexity, decides SKIP/PROCEED/PROCEED_WITH_PLAN.
    Triage,
    /// Designer: produces structured implementation plan from issue + triage output.
    Architect,
    /// Engineer: writes code, creates PR. Receives plan as context.
    Implementer,
    /// QA: writes and runs tests. Operates on the branch state after implementation.
    Tester,
    /// Auditor: reviews PR diff. Returns LGTM/NEEDS_WORK/BLOCKED.
    Reviewer,
    /// Coordinator: synthesizes outputs from multiple agents into final decision.
    Synthesizer,
}
```

### 2.2 Agent Specifications

| Role | Model | Tools | Input | Output | Cost/Task |
|---|---|---|---|---|---|
| **Supervisor** | — (orchestration code, not an LLM) | — | Task request | Phase routing decisions | $0 |
| **Triage** | Haiku | Read-only: `Bash(read)`, `Glob`, `Grep`, `Read` | Issue body + repo context | `TriageDecision { decision, complexity, notes }` | ~$0.001 |
| **Architect** | Sonnet | Read-only: `Bash(read)`, `Glob`, `Grep`, `Read` | Issue + triage output | `ArchitectPlan { approach, files, risks, test_strategy }` | ~$0.005 |
| **Implementer** | Sonnet/Opus | Full: `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep` | Issue + plan | Code changes + PR | ~$0.02 |
| **Tester** | Sonnet | Full: `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep` | Branch state + plan | Test files + coverage report | ~$0.005 |
| **Reviewer** | Codex/Sonnet | Read-only: `Bash(read)`, `Glob`, `Grep`, `Read` | PR diff + plan | `ReviewVerdict { verdict, comments, blocking_issues }` | ~$0.005 |
| **Synthesizer** | Sonnet | None (text-only) | Multiple agent outputs | `SynthesisResult { decision, rationale }` | ~$0.002 |

### 2.3 Supervisor Implementation

The Supervisor is not an LLM — it is orchestration code that routes phases to agents:

```rust
struct Supervisor {
    task_id: TaskId,
    state: TaskState,
    agents: HashMap<AgentRole, Arc<dyn CodeAgent>>,
    branch_lock: BranchLockManager,
}

impl Supervisor {
    async fn run(&mut self) -> anyhow::Result<()> {
        // Phase 1: Triage
        let triage = self.dispatch(AgentRole::Triage, &self.build_triage_prompt()).await?;
        let decision = parse_triage_decision(&triage.output)?;

        match decision.decision {
            Decision::Skip => return self.complete("Skipped by triage"),
            Decision::Proceed => { /* skip plan */ }
            Decision::ProceedWithPlan => {
                // Phase 2: Architect
                let plan = self.dispatch(AgentRole::Architect, &self.build_plan_prompt(&triage.output)).await?;
                self.state.plan_output = Some(plan.output.clone());
            }
        }

        // Phase 3+4: Implement and Test (parallel)
        let (impl_result, test_result) = {
            let _lock = self.branch_lock.acquire(AgentRole::Implementer).await;
            let impl_handle = self.dispatch_async(AgentRole::Implementer, &self.build_impl_prompt());

            // Wait for implementer to push, then start tester on the same branch
            let impl_result = impl_handle.await?;
            let _lock = self.branch_lock.acquire(AgentRole::Tester).await;
            let test_result = self.dispatch(AgentRole::Tester, &self.build_test_prompt()).await?;
            (impl_result, test_result)
        };

        // Phase 5: Review (can run in parallel with multiple reviewers)
        let review_result = self.dispatch(AgentRole::Reviewer, &self.build_review_prompt()).await?;
        let verdict = parse_review_verdict(&review_result.output)?;

        match verdict.verdict {
            Verdict::Lgtm => self.complete("LGTM"),
            Verdict::NeedsWork => {
                // Feed review comments back to Implementer
                self.state.turn += 1;
                // ... retry loop
            }
            Verdict::Blocked => self.fail("Blocked by reviewer"),
        }

        Ok(())
    }
}
```

### 2.4 Agent Prompt Templates

Each role gets a focused prompt with minimal context:

**Triage Agent**:
```
You are a Tech Lead evaluating a GitHub issue for implementation feasibility.

Issue: {{ issue_body }}
Repository: {{ repo_slug }}
Recent commits: {{ recent_commits }}

Assess complexity (simple/medium/complex/critical) and decide:
- SKIP: Not actionable, duplicate, or out of scope
- PROCEED: Clear enough to implement directly
- PROCEED_WITH_PLAN: Needs architectural planning first

Output format:
DECISION=<SKIP|PROCEED|PROCEED_WITH_PLAN>
COMPLEXITY=<simple|medium|complex|critical>
NOTES=<brief reasoning>
```

**Reviewer Agent**:
```
You are a code auditor reviewing a pull request. You did NOT write this code.

PR Diff:
{{ pr_diff }}

Implementation Plan (for context):
{{ plan_output }}

Review for:
1. Correctness: Does the code do what the plan describes?
2. Security: SQL injection, XSS, secrets in code, path traversal
3. Performance: O(n^2) loops, unnecessary allocations, missing indexes
4. Style: Consistent with existing codebase patterns

Output format:
VERDICT=<LGTM|NEEDS_WORK|BLOCKED>
COMMENTS=<specific line-level feedback>
BLOCKING_ISSUES=<list of must-fix items, empty if LGTM>
```

Note: The reviewer prompt explicitly states "You did NOT write this code" — this breaks the self-review bias that exists in the current single-agent model.

---

## 3. Handoff Protocol

### 3.1 Structured Markers

Agents communicate via structured output markers that the Supervisor parses:

```
TRIAGE=PROCEED          → Supervisor routes to Implement (or Architect)
PLAN=READY              → Supervisor routes to Implement
IMPLEMENT=PR_CREATED    → Supervisor routes to Review (or Test)
TEST=PASSED             → Supervisor routes to Review
REVIEW=LGTM             → Supervisor marks task Done
REVIEW=NEEDS_WORK       → Supervisor routes back to Implement with comments
```

Markers are versioned to support backward-compatible evolution:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffMarker {
    pub version: u32,  // Currently 1
    pub role: AgentRole,
    pub status: String,
    pub payload: serde_json::Value,
}
```

### 3.2 Git Branch as Shared State

Instead of passing code diffs through messages, agents share state through the git branch:

1. **Implementer** creates a branch `harness/task-{id}` and pushes commits.
2. **Tester** checks out the same branch, adds test files, pushes.
3. **Reviewer** reads the PR diff from GitHub (never checks out locally).

This is **immutable** (commits are append-only), **auditable** (full git history), and **deterministic** (same commit SHA = same state).

### 3.3 Branch Locking

To prevent simultaneous writes to the same branch:

```rust
pub struct BranchLockManager {
    locks: DashMap<String, Arc<Mutex<AgentRole>>>,
    ttl: Duration,  // Auto-release after TTL to prevent deadlocks
}

impl BranchLockManager {
    pub async fn acquire(&self, branch: &str, role: AgentRole) -> BranchLockGuard {
        let lock = self.locks
            .entry(branch.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(role)))
            .clone();
        let guard = lock.lock().await;
        BranchLockGuard { guard, branch: branch.to_string(), manager: self }
    }
}
```

Lock acquisition order:
1. **Implementer** acquires lock → writes code → releases.
2. **Tester** acquires lock → writes tests → releases.
3. **Reviewer** does not need the lock (read-only via GitHub API).

TTL auto-release prevents deadlocks if an agent crashes while holding the lock.

---

## 4. What It Solves

### 4.1 Faster Execution

Parallel execution of independent phases reduces wall-clock time:

| Phase Pair | Sequential (current) | Parallel (swarm) | Speedup |
|---|---|---|---|
| Triage + Plan | 5 min | 5 min (sequential, dependent) | 0% |
| Implement + Test | 15 min | 10 min (overlapping) | 33% |
| Review (2 reviewers) | 6 min | 3 min (parallel) | 50% |
| **Total (complex task)** | **26 min** | **18 min** | **31%** |

For a sprint plan with 10 issues, the compounding effect is larger: triage fan-out alone saves ~40% of the triage phase.

### 4.2 Cheaper Execution

Model specialization reduces cost:

| Phase | Current (Sonnet for all) | Swarm (specialized) | Savings |
|---|---|---|---|
| Triage | $0.005 | $0.001 (Haiku) | 80% |
| Plan | $0.005 | $0.005 (Sonnet) | 0% |
| Implement | $0.020 | $0.020 (Sonnet/Opus) | 0% |
| Test | $0.005 | $0.005 (Sonnet) | 0% |
| Review | $0.005 | $0.005 (Codex) | 0% |
| Synthesis | — | $0.002 (Sonnet) | new cost |
| **Total** | **$0.040** | **$0.038** | **5%** |

The savings are modest for individual tasks but compound across sprint plans (triage is ~25% of total calls).

### 4.3 Better Test Coverage

A dedicated Tester agent:
- Focuses exclusively on test quality (no context pollution from implementation decisions).
- Can be prompted with coverage targets ("achieve 80% line coverage for new code").
- Runs tests independently and reports coverage deltas.

Expected improvement: +5 percentage points coverage on average (based on the separation of concerns — the implementer currently rushes tests to "pass the review").

### 4.4 Fewer Review Iterations

The Architect agent produces a structured plan before implementation begins. This eliminates a common review failure mode: "this approach is wrong, please redesign."

With a plan, the Reviewer validates against the plan, not against their own mental model. Expected reduction: ~30% fewer NEEDS_WORK verdicts on first review.

### 4.5 Fewer Merge Conflicts

Branch locking ensures only one write-capable agent operates on a branch at a time. The current system allows parallel subtasks to write to overlapping files (no file-level conflict detection).

### 4.6 Clearer Debugging

Each agent produces a typed output with structured markers. The Supervisor logs every handoff:

```
[task-042] Triage → PROCEED (complexity=medium, 2.1s, $0.001)
[task-042] Architect → PLAN_READY (3 files, 2 risks, 4.3s, $0.005)
[task-042] Implementer → PR_CREATED (PR #87, 12.1s, $0.020)
[task-042] Tester → PASSED (coverage 84%, 5.2s, $0.005)
[task-042] Reviewer → LGTM (0 blocking issues, 3.0s, $0.005)
```

This is far more informative than the current log: `[task-042] Done (3 rounds, 26 min)`.

---

## 5. What It Costs

### 5.1 Orchestration Overhead

Each handoff between agents incurs:
- Supervisor decision time: ~1-2 sec (parsing output, routing).
- Agent cold start: ~3-5 sec (Claude Code CLI startup per invocation).
- Total per-task overhead: ~9-15 sec for a 5-phase task.

For a 20-minute task, 15 seconds of overhead is negligible (1.25%). For a 2-minute simple task, it is significant (12.5%). Mitigation: simple tasks skip the swarm and use a single agent (the Supervisor decides based on triage complexity).

### 5.2 Race Conditions on Shared Files

Even with branch locking, there are edge cases:
- **Implementer and Tester modify the same file** (e.g., Implementer adds a function, Tester imports it in a test file that also contains existing tests referencing the same module). The branch lock serializes their writes, but the Tester must rebase on the Implementer's changes.
- **Two reviewers comment on the same line**: Not a git conflict (comments are on GitHub, not in code), but may produce contradictory feedback. The Synthesizer resolves this.

### 5.3 Output Parsing

Each agent role has a different output format. The Supervisor must parse structured markers from free-text agent output. This is fragile — if an agent's output format changes, the parser breaks.

Mitigation: Versioned handoff markers (`version: 1`). Output parsers in `prompts.rs` already handle `LGTM`/`FIXED`/`WAITING` extraction; extending to `TRIAGE=PROCEED` is straightforward.

### 5.4 Increased API Calls

| Model | Current (1 agent) | Swarm (6 agents) |
|---|---|---|
| Agent invocations per task | 4 (impl + 3 review rounds) | 5-6 (triage + plan + impl + test + review + synth) |
| Total tokens per task | ~5500 | ~4200 (less context per agent) |
| API calls | 4 | 5-6 |

More API calls, but fewer tokens per call (each agent gets a focused context). Net token reduction: ~23%.

### 5.5 Debugging Complexity

With 6 agents per task, debugging a failure requires tracing through multiple agent logs. The current system has one log per task.

Mitigation: `AgentInteractionLog` table in SQLite:

```sql
CREATE TABLE agent_interactions (
    id INTEGER PRIMARY KEY,
    task_id TEXT NOT NULL,
    role TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cost_usd REAL,
    verdict TEXT,
    error TEXT,
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);
```

---

## 6. Concrete Example: "Add Metrics Endpoint"

### 6.1 Single Agent (Current)

```
[00:00] Agent starts. Reads issue #87: "Add /metrics endpoint with Prometheus format"
[00:30] Agent reads existing code: http.rs, router.rs, lib.rs
[02:00] Agent writes src/metrics.rs (counter, histogram, gauge types)
[05:00] Agent writes http handler, adds route to router
[08:00] Agent runs cargo check, fixes compile errors
[10:00] Agent writes tests in metrics_test.rs
[12:00] Agent runs cargo test, 2 failures, fixes
[14:00] Agent creates PR #88
[14:30] Review round 1: agent reviews its own PR
[16:00] FIXED — agent finds a missing error handler
[16:30] Agent fixes the error handler, pushes
[17:00] Review round 2: agent reviews again
[18:30] LGTM
[18:30] Done

Total: 18.5 min wall-clock
Tokens: ~5500 (accumulated context)
Cost: ~$0.040
Rounds: 2 review rounds (self-review bias: missed the error handler initially)
```

### 6.2 Swarm

```
[00:00] Supervisor receives task for issue #87
[00:01] Supervisor dispatches to Triage Agent (Haiku)

[00:01] Triage Agent reads issue #87
[00:15] Triage Agent: DECISION=PROCEED, COMPLEXITY=medium
        (No plan needed — clear requirements)

[00:16] Supervisor dispatches to Implementer (Sonnet)

[00:16] Implementer reads issue + existing code
[02:00] Implementer writes src/metrics.rs
[05:00] Implementer writes http handler, adds route
[08:00] Implementer runs cargo check, fixes compile errors
[09:00] Implementer creates PR #88
[09:00] Implementer: IMPLEMENT=PR_CREATED

[09:01] Supervisor dispatches to Tester (Sonnet) — acquires branch lock

[09:01] Tester reads branch state + PR diff
[11:00] Tester writes metrics_test.rs with 12 test cases
[12:00] Tester runs cargo test, all pass. Coverage: 87%
[12:30] Tester pushes test commit
[12:30] Tester: TEST=PASSED, COVERAGE=87%

[12:31] Supervisor dispatches to Reviewer (Codex) — read-only, no lock needed

[12:31] Reviewer reads PR diff (only the diff, not the full implementation context)
[14:00] Reviewer: VERDICT=NEEDS_WORK
        COMMENTS: "Missing error handler for metrics collection failure"
        BLOCKING_ISSUES: ["metrics::collect() returns Result but handler unwraps"]

[14:01] Supervisor dispatches back to Implementer with review comments

[14:01] Implementer reads review comments (focused context: just the comments + relevant code)
[15:00] Implementer adds error handler, pushes
[15:00] Implementer: IMPLEMENT=UPDATED

[15:01] Supervisor dispatches to Reviewer (Codex)

[15:01] Reviewer reads updated diff
[16:00] Reviewer: VERDICT=LGTM

[16:01] Supervisor marks task Done

Total: 16 min wall-clock
Tokens: ~4200 (focused context per agent)
Cost: ~$0.038
Rounds: 1 rework round (reviewer caught the error on first pass — no self-review bias)
```

### 6.3 Comparison Summary

| Metric | Single Agent | Swarm | Delta |
|---|---|---|---|
| Wall-clock time | 18.5 min | 16.0 min | -14% |
| Total tokens | ~5500 | ~4200 | -24% |
| Cost | $0.040 | $0.038 | -5% |
| Review rounds | 2 | 1 | -50% |
| Test coverage | ~75% | ~87% | +12pp |
| Agent invocations | 4 | 7 | +75% |

The swarm trades more API calls for fewer tokens, better review quality, and higher test coverage. The wall-clock improvement comes from the Tester running in parallel with part of the implementation.

---

## 7. Comparison with Existing Frameworks

### 7.1 Framework Comparison Table

| Dimension | CrewAI | OpenAI Swarm | AutoGen | Harness (proposed) |
|---|---|---|---|---|
| **State sharing** | In-memory dict | None (stateless) | Chat history | Git commits (immutable, auditable) |
| **Handoff mechanism** | Function call return | Return value with `agent` field | Natural language in chat | Structured markers (versioned) |
| **Concurrency** | Sequential by default | Sequential | Async chat | Async with branch locking |
| **Persistence** | None (in-memory) | None | Chat log | SQLite + git history |
| **Tool access** | Python functions | Python functions | Python functions | CLI tools (Bash, Read, Write, Edit) |
| **Recovery** | None | None | Replay chat | Checkpoint + event replay |
| **GitHub integration** | Via API wrapper | None | Via API wrapper | Native (git branch is the state) |
| **Model selection** | Per-agent config | Per-agent config | Per-agent config | Per-role config with cost optimization |

### 7.2 Harness's Unique Advantage: Git Branch as Shared State

Every other framework uses in-memory state (dicts, chat histories, return values) for inter-agent communication. This state is:
- **Volatile**: Lost on crash.
- **Opaque**: No audit trail of who wrote what.
- **Non-deterministic**: Concurrent writes can conflict without detection.

Harness uses git commits as shared state:
- **Immutable**: A commit SHA is permanent. No agent can modify another agent's output.
- **Auditable**: `git log` shows exactly what each agent contributed (commit author metadata).
- **Deterministic**: Same SHA = same state. No race conditions on reads.
- **Recoverable**: On crash, the branch state survives. The supervisor resumes from the last known commit.

This is not just a technical advantage — it aligns with how software engineers naturally work. The branch *is* the artifact. Agents contribute to it just like human developers do.

---

## 8. Implementation Roadmap

### Phase 0: Foundation (Week 1-2)

**Scope**: Type definitions and Supervisor router.

```rust
// New file: crates/harness-core/src/agent_role.rs
pub enum AgentRole { Supervisor, Triage, Architect, Implementer, Tester, Reviewer, Synthesizer }

// Extension to TaskState
pub struct TaskState {
    // ... existing fields ...
    pub agent_interactions: Vec<AgentInteraction>,
}

pub struct AgentInteraction {
    pub role: AgentRole,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub verdict: Option<String>,
    pub error: Option<String>,
}
```

Supervisor router: a function that maps `(TaskPhase, TriageDecision)` to `AgentRole`. No LLM involved — pure dispatch logic.

**Deliverables**: `AgentRole` enum, `AgentInteraction` struct, Supervisor dispatch function, unit tests.

### Phase 1: Triage Agent (Week 2-3)

**Scope**: Extract triage into a dedicated agent with Haiku model.

Currently, triage runs as a phase within the main agent session. Extract it to a separate `execute()` call with:
- Read-only tool set.
- Haiku model (via `execution_phase: Some(ExecutionPhase::Triage)` + `ReasoningBudget` config).
- Structured output parsing for `DECISION`, `COMPLEXITY`, `NOTES`.

**Deliverables**: Triage prompt template, output parser, integration test showing Haiku triage + Sonnet implementation.

### Phase 2: Architect Agent (Week 3-4)

**Scope**: Dedicated planning agent for PROCEED_WITH_PLAN issues.

The Architect receives the issue + triage output and produces a structured plan:
```
APPROACH: <1-2 sentences>
FILES: <list of files to create/modify>
RISKS: <potential issues>
TEST_STRATEGY: <what to test and how>
```

The plan is passed as context to the Implementer. This replaces the current `plan_output` field which is populated by the same agent that implements.

**Deliverables**: Architect prompt template, plan output parser, plan-to-context adapter.

### Phase 3: Parallel Implement + Test (Week 4-5)

**Scope**: Run Implementer and Tester on the same branch with locking.

This is the most complex phase. The Implementer pushes code; the Tester pulls and writes tests. Branch locking ensures they do not write simultaneously.

Implementation strategy:
1. Implementer runs to completion (creates PR).
2. Tester checks out the branch, writes tests, pushes.
3. If tests fail, Tester reports failures. Supervisor routes back to Implementer with test output.

Note: True parallelism (Implementer and Tester writing simultaneously) is deferred to a future phase due to merge conflict risk. Phase 3 is sequential but with dedicated agents.

**Deliverables**: Branch lock manager, Tester prompt template, test coverage parser, integration test.

### Phase 4: Reviewer + Synthesizer (Week 5-6)

**Scope**: Dedicated Reviewer agent, optionally with parallel reviewers and synthesis.

Base case: Single Reviewer agent with Codex model, receiving only the PR diff (no implementation context).

Extended case: Two Reviewers (Claude + Codex) run in parallel, Synthesizer combines their verdicts. This mirrors the existing `periodic_reviewer.rs` pattern.

**Deliverables**: Reviewer prompt template (no self-review bias), Synthesizer prompt template, verdict parser, `AgentInteractionLog` table.

### Phase 5: Observability (Week 6-7)

**Scope**: Dashboard integration, interaction logs, cost tracking.

- `GET /tasks/{id}/interactions` endpoint returning `Vec<AgentInteraction>`.
- Dashboard UI showing per-agent timeline (Gantt chart style).
- Cost breakdown per role per task.
- Aggregate statistics: average cost by role, average review rounds by complexity.

**Deliverables**: HTTP endpoints, dashboard data model, cost aggregation queries.

### Phase 6: Benchmarking and A/B Testing (Week 7-8)

**Scope**: Compare single-agent vs. swarm on a representative task set.

Methodology:
1. Select 20 tasks from recent sprint plans (mix of simple/medium/complex).
2. Run each task with single-agent (current) and swarm (new).
3. Compare: wall-clock time, cost, review rounds, test coverage, final code quality.

A/B testing: Add a `swarm_enabled` flag to `CreateTaskRequest`. Default false. Users opt in per task or per project.

**Deliverables**: Benchmark framework, comparison report, A/B flag implementation.

---

## 9. Existing Code Foundation

The swarm architecture is not starting from zero. Several existing components serve as building blocks:

### 9.1 parallel_dispatch.rs

Already runs multiple agent executions concurrently via `tokio::spawn` + `join_all`:

```rust
pub fn decompose(prompt: &str) -> Vec<String> { ... }

pub struct SubtaskResult {
    pub index: usize,
    pub task_id: TaskId,
    pub success: bool,
}
```

This is the foundation for fan-out. Extending it to support role-based fan-out (Implementer + Tester in parallel) is straightforward.

### 9.2 periodic_reviewer.rs

Already spawns Claude + Codex in parallel and synthesizes their outputs:

```rust
// Simplified from periodic_reviewer.rs
let claude_handle = dispatch_review(state, "claude", &prompt);
let codex_handle = dispatch_review(state, "codex", &prompt);
let (claude_result, codex_result) = tokio::join!(claude_handle, codex_handle);
let synthesis = synthesize(claude_result, codex_result);
```

This is the prototype for the Reviewer + Synthesizer pattern. The main adaptation needed: make it part of the task pipeline instead of a standalone periodic process.

### 9.3 agent.rs (CodeAgent trait + AgentRequest)

The `CodeAgent` trait and `AgentRequest` struct already support per-agent model selection and tool restriction:

```rust
pub trait CodeAgent: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Vec<Capability>;
    async fn execute(&self, req: AgentRequest) -> Result<AgentResponse>;
    async fn execute_stream(&self, req: AgentRequest, tx: mpsc::Sender<StreamItem>) -> Result<()>;
}

pub struct AgentRequest {
    pub prompt: String,
    pub allowed_tools: Vec<String>,  // Role-based tool restriction
    pub model: Option<String>,       // Role-based model selection
    pub execution_phase: Option<ExecutionPhase>,  // Phase-based model via ReasoningBudget
    pub env_vars: HashMap<String, String>,
    // ...
}
```

Each swarm role maps to a `CodeAgent::execute()` call with role-specific `allowed_tools` and `model`. No new trait is needed.

### 9.4 CapabilityProfile

The `CapabilityProfile` enum already defines tool restriction sets:

```rust
pub enum CapabilityProfile {
    Full,           // All tools
    ReadOnly,       // Bash(read), Glob, Grep, Read
    ReviewOnly,     // Same as ReadOnly
    // ...
}
```

The Triage, Architect, and Reviewer roles use `ReadOnly`. The Implementer and Tester use `Full`. This mapping already exists in the codebase.

### 9.5 Gap Assessment

| Component | Exists | Needs Adaptation |
|---|---|---|
| Parallel agent execution | Yes (`parallel_dispatch.rs`) | Add role-based dispatch |
| Multi-reviewer synthesis | Yes (`periodic_reviewer.rs`) | Integrate into task pipeline |
| Per-agent model selection | Yes (`AgentRequest.model`) | Add role→model mapping config |
| Tool restriction per role | Yes (`CapabilityProfile`) | Map roles to profiles |
| Branch locking | No | New: `BranchLockManager` |
| Structured handoff parsing | Partial (`prompts.rs` parsers) | Extend for all role outputs |
| Agent interaction logging | No | New: `AgentInteractionLog` table |
| Supervisor routing logic | No | New: dispatch function |

Harness is closer to a swarm architecture than it appears. The primary gaps are orchestration (Supervisor), locking (BranchLockManager), and observability (interaction logging). The agent infrastructure — execution, streaming, tool restriction, model selection — is already in place.

---

## 10. Open Questions

1. **Swarm threshold**: At what complexity level should the Supervisor activate the swarm? Simple tasks should use a single agent (lower overhead). Proposal: Triage always runs (it is cheap); if `COMPLEXITY=simple`, skip the swarm and use a single Implementer.

2. **Agent identity**: Should each role use a separate Claude Code session (cold start per role) or share a session with tool restrictions? Separate sessions provide stronger isolation but incur cold-start overhead (~3-5 sec).

3. **Review escalation**: If the Reviewer says BLOCKED, should the Supervisor escalate to a human? Or retry with a different Reviewer (Codex → Claude)? Or synthesize multiple Reviewer opinions?

4. **Tester independence**: Should the Tester see the implementation diff (to write targeted tests) or only the plan (to write spec-based tests)? Seeing the diff risks testing the implementation rather than the specification.

5. **Cost budget allocation**: With a per-task `max_budget_usd`, how should the Supervisor allocate budget across roles? Proposal: 5% triage, 10% plan, 50% implement, 15% test, 15% review, 5% synthesis.

6. **Failure attribution**: When a task fails after 3 rework rounds, which agent is responsible? The Implementer (bad code)? The Architect (bad plan)? The Reviewer (bad feedback)? The interaction log provides data, but attribution requires analysis.
