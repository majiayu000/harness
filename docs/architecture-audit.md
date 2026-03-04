# Harness Architecture Audit — Declaration-Execution Gap

> Date: 2026-03-04
> Scope: Full codebase verification of 11 architectural findings + root cause analysis

## Executive Summary

Harness has comprehensive **declaration** (25+ rules, 6 signal types, 4-dimension quality grading, interceptor framework, notification protocol) but near-zero **runtime enforcement**. The system can observe deeply and report faithfully, but nothing actually blocks or corrects.

**Core problem: too much design, too few closed loops.**

## Verified Findings

### Critical

| # | Finding | Evidence | Severity |
|---|---------|----------|----------|
| H1 | **Config vs runtime path divergence** | `config.rs:38` defaults to `~/Library/Application Support/harness` (macOS); `http.rs:31` hardcodes `~/.local/share/harness`. Two different default paths, config never used by `build_app_state`. | Critical (U-11 violation) |

### High

| # | Finding | Evidence | Severity |
|---|---------|----------|----------|
| H2 | **Rules loaded but never enforced** | `build_app_state` calls `load_builtin()` (40+ rules parsed), but `scan()` only executes guards, and guards list is always empty. Rules are documentation, not enforcement. | High |
| H4 | **Notifications declared but never sent** | `notifications.rs` defines 6 notification types + `RpcNotification` envelope. stdio.rs is pure request-response. No SSE/WebSocket endpoint exists. Dead code. | High |

### Medium

| # | Finding | Evidence | Severity |
|---|---------|----------|----------|
| H3 | **GC adopt has no path validation** | `gc_agent.rs:129` calls `fs::write(&artifact.target_path, ...)` without validation. Currently safe because `parse_artifacts()` hardcodes `.harness/drafts/` path, but `adopt()` itself has no boundary check. | Medium (design flaw, not currently exploitable) |
| H5 | **GcRun ignores request context** | `gc.rs:13` hardcodes `PathBuf::from(".")`. Ignores any project context from the RPC request. | Medium |
| M1 | **Thread persistence dual channel** | `ThreadManager` has internal `db: Option<ThreadDb>` (always None in practice) + persist methods. `AppState` holds a separate `thread_db: Option<ThreadDb>`. Handlers use `state.thread_db` directly. ThreadManager's built-in persistence is dead code. | Medium (RS-12 violation) |
| M2 | **Skills not reloaded on startup** | `http.rs:84` calls `with_persist_dir()` but not `discover()`. Skills saved to disk are silently lost on restart. | Medium (U-23 violation) |
| M3 | **Error codes uniformly INTERNAL_ERROR** | Thread not found, draft not found, validation failure all return `INTERNAL_ERROR`. No semantic error code mapping. | Medium |
| M4 | **No agent selection mechanism** | `create_task` always uses `default_agent()`. `TaskComplexity` enum and `AgentRegistry.dispatch()` exist but are never called. | Medium |

### Low

| # | Finding | Evidence | Severity |
|---|---------|----------|----------|
| L1 | **ExecPlan is memory-only** | `http.rs:22` uses `HashMap<ExecPlanId, ExecPlan>`. Lost on restart. | Low |
| L2 | **EventStore reads entire file per query** | `event_store.rs:38` loads full JSONL file into memory, filters, then truncates. | Low |

## Declaration vs Enforcement Map

| Component | Declared | Enforced at Runtime | Gap |
|-----------|----------|---------------------|-----|
| Rules (7 files, 25+ rules) | Comprehensive | On-demand scan only, guards empty | 100% gap |
| Interceptors (TurnInterceptor trait) | Framework complete | `interceptors: vec![]` | 100% gap |
| Quality Grading (4 dimensions) | Defined + tested | Never called by any handler | 100% gap |
| Signal Detection (6 types) | Active + tested | Manual RPC trigger only | ~50% gap |
| Event Logging (JSONL) | Active for rules + PR reviews | Limited event types captured | ~30% gap |
| Notifications (6 types) | Protocol defined | Never sent | 100% gap |
| GC Cycle (signal → draft → adopt) | Active | No auto-trigger, manual only | ~50% gap |

## Root Cause Analysis

### 1. Bottom-Up Development Without Integration Closure

Each crate was developed and tested in isolation:

- `RuleEngine` tests: rules parsing correct
- `SignalDetector` tests: threshold detection correct
- `QualityGrader` tests: scoring formula correct
- `TurnInterceptor` tests: trait dispatch correct

All unit tests pass. But **no end-to-end test** verifies: "start server -> submit task -> does rule checking happen? does interceptor block?"

Individual components are correct. The assembly is disconnected.

### 2. Framework-First Mentality

Phase 1 produced `TurnInterceptor` trait + `InterceptResult` enum + AppState integration. This is scaffolding, not functionality.

```rust
// Built the framework
pub trait TurnInterceptor: Send + Sync {
    async fn pre_execute(&self, req: &AgentRequest) -> InterceptResult;
}

// But never instantiated a single implementation
interceptors: vec![], // <- empty forever
```

AI-assisted development makes generating abstractions nearly free. This creates rapid accumulation of "looks complete" capabilities that are actually inert.

### 3. Config as Documentation, Not Runtime

`HarnessConfig` defines a complete configuration model (server/agents/gc/rules/observe). `build_app_state` independently computes its own paths. Two code paths were written in different sessions with no integration test to catch the divergence.

- `config.rs` — macOS default: `~/Library/Application Support/harness`
- `http.rs` — hardcoded: `~/.local/share/harness`

Both paths are internally correct. They just don't know about each other.

### 4. Observation-First Architecture Defers Enforcement Forever

Harness philosophy: observe -> generate draft -> human approval -> then execute. This is reasonable, but in practice every mechanism stopped at "observe":

| Stage | Status |
|-------|--------|
| EventStore records events | Working |
| SignalDetector detects anomalies | Working (manual trigger) |
| QualityGrader scores quality | Never called |
| Interceptor blocks execution | Framework exists, zero implementations |
| Notification pushes status | Types defined, never sent |

### 5. Multi-Session Fast Development Without Backtracking

Multiple sessions pushed forward rapidly: Phase 0 split -> Phase 1 interceptor -> security hardening -> PR submission. Every session **added new capabilities forward**. No session went back to verify cross-module integration or close existing loops.

### 6. Self-Referential Irony

Harness's own built-in rules detect exactly the issues it suffers from:

| Built-in Rule | Harness's Own Violation |
|---------------|------------------------|
| U-11: Multi-binary default path inconsistency | config.rs vs http.rs path divergence |
| RS-12: Same responsibility, dual systems | ThreadManager.db vs AppState.thread_db |
| RS-13: Action functions lack state side effects | QualityGrader scores but doesn't persist/act |
| GP-04: Observable operations | Notifications declared but never sent |
| U-23: No silent degradation | Skill startup silently loses disk data |

But these rules have no guard scripts to enforce them. `RuleEngine.scan()` depends on guards, guards list is empty. Rules are human-readable documents, not machine-executable checks.

## Architectural Principle: Closed Loops Over Coverage

### The Anti-Pattern

```
Declaration Layer  ████████████████████████  (rich)
Enforcement Layer  ████                      (near zero)
```

Adding more declaration nodes does not improve enforcement. Adding a 6th subsystem to 5 disconnected subsystems produces 6 disconnected subsystems.

### The Correct Approach

**Less breadth, more depth per node. Wire 2 loops end-to-end before starting a 3rd.**

```
Wrong:  □ -> □ -> □ -> □ -> □      (5 nodes, 0 powered)
Wrong:  □ -> □ -> □ -> □ -> □ -> □  (6 nodes, 0 powered)
Right:  ■ -> ■                      (2 nodes, 2 powered)
```

### Target Closed Loops (Priority Order)

**Loop 1: Rules -> Block**

```
Task submitted -> Interceptor (concrete instance, registered) -> Block/Pass -> Agent executes
```

Requirements:
- One concrete `ContractValidator` interceptor implementation
- Registered in `build_app_state` (not empty vec)
- Integration test: submit task -> interceptor blocks -> task fails

**Loop 2: Events -> Feedback**

```
Agent executes -> EventStore logs -> SignalDetector detects -> Auto-trigger GC
```

Requirements:
- Threshold-based automatic GC trigger (not manual RPC)
- Integration test: generate events -> threshold crossed -> GC cycle runs

Everything else (QualityGrader, Notifications, ExecPlan persistence) waits until these two loops are stable.

## Specific Fixes Required

### Immediate (Close existing gaps)

1. **Delete `data_dir()` function in http.rs**, use `server.config.server.data_dir` as single source of truth
2. **Call `discover()` after `with_persist_dir()`** in `build_app_state` to reload persisted skills
3. **Collapse Thread persistence** to single channel: either ThreadManager.db or AppState.thread_db, not both

### Short-Term (Close Loop 1)

4. **Implement one concrete interceptor** (e.g., prompt length limit, file count gate) and register it in `build_app_state`
5. **Add integration test**: task submission -> interceptor evaluation -> block/pass verified

### Medium-Term (Close Loop 2)

6. **Add threshold-triggered GC** in task_executor post-execution hook: if event count since last GC exceeds threshold, auto-run GC cycle
7. **Wire GcRun to use thread's project_root** instead of hardcoded "."

### Deferred (After loops are stable)

8. QualityGrader integration into GC scheduling
9. Notification streaming via SSE/WebSocket
10. ExecPlan persistence to SQLite
11. EventStore migration to SQLite

## Verification Checklist

For any new subsystem addition, verify before merging:

- [ ] Is there at least one concrete implementation (not just a trait)?
- [ ] Is the implementation registered/wired at startup (not empty vec)?
- [ ] Is there an integration test from entry point to observable effect?
- [ ] Does the new code use existing config values (not hardcoded paths)?
- [ ] Run `cargo +nightly udeps` — are any new pub functions unused?
