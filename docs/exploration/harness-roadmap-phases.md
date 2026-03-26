# Harness Evolution Roadmap

> Based on exploration session 2026-03-24. Principle: **don't over-engineer** — model capabilities are improving fast. Invest heavily in infrastructure (always needed), invest lightly in model-weakness-compensation (may become unnecessary).

## Investment Tiers

| Tier | Category | Items | Rationale |
|------|----------|-------|-----------|
| **Heavy** | Infrastructure (always needed) | Event sourcing, crash recovery, non-LLM verification | Physical problems: servers crash, processes die. Model-independent |
| **Light** | Cost optimization (needed while price gaps exist) | Triage complexity → dynamic max_rounds/skip_review | Simple if/else, no framework |
| **Minimal** | Model weakness compensation (may become unnecessary) | Impasse detection, complex multi-agent orchestration | Better models self-correct; keep it to a few lines of detection code |

---

## Phase 0: Current State

- Single agent (Claude Code CLI) per task
- Pipeline: Triage → (optional Plan) → Implement → Review
- Triage outputs: PROCEED / PROCEED_WITH_PLAN / SKIP / NEEDS_CLARIFICATION
- Dual-model review: agent review (Codex) + external review bot (Gemini)
- In-memory TaskState (DashMap) + TaskDb (SQLite) + events.jsonl
- Known issues: 54% failures from review loop exhaustion, 37% from server restart state loss

---

## Phase 1: Foundation (4-6 weeks)

### 1.1 Event Sourcing Formalization [HEAVY — always needed]

**Problem**: Server restart loses 37% of in-progress tasks. TaskState is in-memory; events.jsonl exists but isn't used for recovery.

**Change**: Make events.jsonl the source of truth. On startup, replay events to reconstruct TaskState.

**Scope**:
- Define `TaskEvent` enum: `Created`, `PhaseChanged(phase)`, `StatusChanged(status, turn)`, `RoundCompleted(round, result)`, `AgentStarted(phase)`, `AgentCompleted(phase, output_summary)`, `Completed`, `Failed(reason)`
- Replace direct `mutate_and_persist` calls with `emit_event` that: (1) appends to events.jsonl, (2) updates in-memory state, (3) persists to TaskDb
- On startup: scan events.jsonl for incomplete tasks → replay → resume from last checkpoint
- Checkpoint = last successfully completed phase (Triage done? Plan done? Implement done? Review round N done?)

**Files**: task_executor.rs, task_runner.rs, task_db.rs, new: event_replay.rs
**Risk**: Medium — touches core execution path
**Model-proof**: Yes — servers will always crash

### 1.2 Triage Complexity Output [LIGHT — cost optimization]

**Problem**: All tasks get same max_rounds regardless of difficulty. Simple tasks waste rounds; complex tasks may need more.

**Change**: Add `COMPLEXITY=low/medium/high` to triage output. Use it to set dynamic parameters.

**Scope**:
- Extend `TriageDecision` parsing to also extract `COMPLEXITY` tag
- Add complexity field to triage prompt template
- Map complexity to parameters:
  - `low`: max_rounds=2, skip agent review
  - `medium`: max_rounds=4 (default)
  - `high`: max_rounds=8, enable parallel dispatch if available

**Files**: prompts.rs (triage prompt + parser), task_executor.rs (use complexity in run_task)
**Risk**: Low — additive change, fallback to medium if tag missing
**Model-proof**: Partially — cost optimization matters while models aren't free

### 1.3 Impasse Detection [MINIMAL — model weakness compensation]

**Problem**: Agents get stuck in loops, repeating the same failing approach until rounds exhaust.

**Change**: Detect consecutive identical errors in review loop. On 3rd repetition, inject a "step back and try a different approach" prompt or mark as needs-human-help.

**Scope**:
- In the review loop (task_executor.rs line 973), track last N error signatures (hash of error message)
- If same signature appears 3 times: inject strategy-change prompt into next round
- If same signature appears 5 times: mark task as Failed with "impasse detected" reason instead of burning remaining rounds

**Files**: task_executor.rs (review loop only, ~20 lines of detection code)
**Risk**: Very low — just pattern matching on existing data
**Model-proof**: No — better models get stuck less. But the code is trivial to keep.

---

## Phase 2: Robustness (6-10 weeks, depends on Phase 1)

### 2.1 Checkpoint-Based Crash Recovery [HEAVY — always needed]

**Problem**: Even with event sourcing, resuming mid-phase is complex. Need explicit checkpoints.

**Change**: After each completed phase, write a checkpoint that contains enough state to resume from the next phase.

**Scope**:
- Checkpoint after Triage: save triage_output + decision + complexity
- Checkpoint after Plan: save plan_output
- Checkpoint after Implement: save pr_url + pr_number
- On restart with checkpoint: skip completed phases, resume from next one
- For review loop: checkpoint after each completed round (save round number + results)

**Files**: task_db.rs (checkpoint table), task_executor.rs (checkpoint writes), task_runner.rs (checkpoint reads on restart)
**Depends on**: Phase 1.1 (event sourcing)
**Model-proof**: Yes — always needed

### 2.2 HTN Task Decomposition [LIGHT — formalize existing pattern]

**Problem**: Sprint planner creates tasks but doesn't model dependencies or methods. Complex tasks could benefit from hierarchical decomposition.

**Change**: Formalize the existing sprint plan pattern with explicit subtask dependencies.

**Scope**:
- Add `parent_task_id` and `depends_on: Vec<TaskId>` fields to CreateTaskRequest
- Sprint planner output parsed into a dependency graph (simple: A before B, C parallel with D)
- Task scheduler respects dependencies: don't start task B until task A completes
- Display dependency tree in task list

**Files**: protocol (methods.rs), task_runner.rs (scheduling), task_db.rs (parent/depends fields)
**Risk**: Medium — new scheduling logic
**Model-proof**: Partially — task ordering is infrastructure, but decomposition quality depends on model

### 2.3 Review Strengthening [LIGHT — process guarantee]

**Problem**: LLM-only review has a fundamental gaming risk (OpenAI misbehavior paper). Need non-LLM verification anchors.

**Change**: Add hard verification gates before/after agent review.

**Scope**:
- Before marking LGTM: verify `cargo test` / `npm test` passes (already in prompt, but make it structural — check exit code in harness, not just trust agent)
- Before marking LGTM: verify `cargo check` / type check passes
- Track: did the PR diff actually change files related to the issue? (simple heuristic, not complex analysis)

**Files**: task_executor.rs (add verification step after review verdict)
**Risk**: Low — additive gates
**Model-proof**: Yes — tests and type checks are always useful

---

## Phase 3: Future (only if needed)

### 3.1 A2A Protocol Support

Only needed if Harness agents need to collaborate with agents outside the Harness ecosystem (e.g., a Salesforce agent, an SAP agent). Not needed for current single-project use case.

### 3.2 Graph-Based Orchestration

Only needed if task flows become significantly more complex (10+ conditional branches, parallel fan-out/fan-in). Current linear pipeline + optional plan is sufficient for most GitHub issue workflows.

### 3.3 Multi-Model Routing

Route different phases to different models (cheap model for triage, strong model for implementation). Only valuable if price gaps between models remain significant. Currently Harness already supports separate agent for review (Codex) vs implementation (Claude).

---

## Summary: Lines of Code Estimate

| Item | LOC | Priority | Model-proof |
|------|-----|----------|-------------|
| 1.1 Event sourcing | ~300 | P0 | Yes |
| 1.2 Triage complexity | ~50 | P1 | Partially |
| 1.3 Impasse detection | ~20 | P2 | No |
| 2.1 Checkpoint recovery | ~200 | P0 | Yes |
| 2.2 HTN decomposition | ~150 | P1 | Partially |
| 2.3 Review verification gates | ~50 | P1 | Yes |
| **Total Phase 1** | **~370** | | |
| **Total Phase 1+2** | **~770** | | |
