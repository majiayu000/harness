# Harness Design Cleanup Plan

## Purpose

Harness has accumulated several good abstractions that still coexist with older execution paths. The cleanup goal is not a rewrite; it is to make one path canonical per domain and delete or isolate the alternatives.

## Current architectural pressure points

### 1. Server crate owns too many concerns

`harness-server` currently contains transport routing, task execution, persistence access, workspace lifecycle, intake, runtime host leasing, rule enforcement, review loops, dashboard APIs, and reconciliation. This makes it the highest-coupling crate in the workspace.

Direction: keep HTTP/JSON-RPC transport in `harness-server`, but move domain orchestration behind service traits and smaller domain modules.

### 2. Service layer is partially adopted

`AppState` exposes both service traits and raw stores/engines. This lets handlers choose between the intended service boundary and direct concrete access.

Direction: handlers should call services; services should own direct access to stores, engines, queues, and registries.

### 3. Agent execution had name-based behavior

The runtime previously distinguished Codex from Claude inside turn execution by checking the adapter name. This is brittle because agent semantics belong at registration time, not in execution control flow.

Direction: use an adapter execution strategy:

- `ControlOnly`: adapter is available for side-channel control, while `CodeAgent` executes turns.
- `ExecuteTurns`: adapter owns the turn lifecycle.

Claude is currently `ControlOnly`; Codex is currently `ExecuteTurns`.

### 4. Rule enforcement is not a fully closed loop

Rules, guards, interceptors, and events exist, but full runtime enforcement depends on affected-file telemetry. Host-side git inspection is intentionally disabled, and the full pre-turn `RuleEnforcer` is not active in the default interceptor stack.

Direction: either make runtime enforcement complete with reliable file telemetry and source-aware filtering, or present rules as manual/on-demand scans. Avoid code comments or docs that imply enforcement is stronger than the active runtime path.

### 5. Task state mixes multiple domains

`TaskState` includes lifecycle, review metadata, intake links, dependency graph, workspace lease data, scheduler authority, and runtime-host lease state.

Direction: keep the persisted record stable, but move mutation logic behind domain-specific transition APIs:

- task lifecycle;
- scheduler authority;
- workspace lease;
- review state;
- intake/source link;
- runtime host lease.

### 6. Workspace lifecycle is a hotspot

Workspace management combines git worktree operations, owner records, stale cleanup, key derivation, hooks, reconciliation, and tests. There is also an unfinished `workspace/` split in the source tree.

Direction: finish the split or delete the unfinished split. The target layout should separate git utilities, owner records, lifecycle acquisition/release, reconciliation, key derivation, and hooks.

### 7. Error mapping is inconsistent

The protocol defines semantic JSON-RPC errors, but several handlers still map validation and client input failures to `INTERNAL_ERROR`.

Direction: route handler failures through semantic helpers such as invalid params, validation, not found, storage, agent, and internal errors.

## Refactoring rule

For each cleanup, verify four links before merging:

1. Declaration: the abstraction exists.
2. Execution: startup/runtime code actually uses it.
3. Validation: tests cover the entry point and observable effect.
4. Removal: old duplicate paths are deleted or clearly marked as compatibility shims.
