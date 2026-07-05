# Tech Spec

## Linked Issue

GH-1463

## Product Spec

See `specs/GH1463/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Background root | `crates/harness-server/src/http/background.rs` | 3,542 lines on current main; contains shared recovery helpers, dependency watchers, runtime command dispatch, runtime profile resolution, PR feedback sweeping, runtime job workers, and startup recovery flows. | This is the oversized surface to split. |
| HTTP siblings | `crates/harness-server/src/http/*.rs` | Sibling modules call items through `http::background` and `pub(super)` visibility. | The split must preserve the module surface and avoid moving call sites. |
| File-size policy | `CLAUDE.md` | Grants `**/harness-server/src/http/background.rs` a 3,600-line U-16 exemption. | The exemption must be removed or reduced once the root is thin. |
| Server behavior | `crates/harness-server` tests and workspace checks | Existing tests cover route startup, runtime command dispatch, task recovery, and background worker interactions. | Verification must prove the move did not break compile-time or test-covered behavior. |

## Proposed Design

1. Keep `background.rs` as the module entrypoint.
   - Declare child modules under `crates/harness-server/src/http/background/`.
   - Re-export moved items that are currently used through `http::background`.
   - Keep root-only glue minimal.
2. Split by existing concern groups.
   - `support.rs`: shared parse helpers, recovered task request construction,
     reviewer resolution, background task failure, prompt orphan recovery
     helpers, and startup recovery readiness helpers.
   - `awaiting_deps.rs`: `spawn_awaiting_deps_watcher` and dependency-ready
     task dispatch.
   - `runtime_command_dispatch.rs`: runtime command dispatch ticks,
     command-specific project/root helpers, gate fact hashing, dispatch metrics,
     and outbox status transitions.
   - `runtime_profiles.rs`: runtime kind/profile resolution, profile selector
     creation, manifest persistence, approval policy mapping, and profile
     override handling.
   - `pr_feedback.rs`: PR feedback sweep ticks, auto-merge request recovery,
     and `spawn_runtime_pr_feedback_sweeper`.
   - `runtime_workers.rs`: runtime worker lease state, worker tick spawning,
     open-slot calculation, shutdown handling, and job worker supervision.
   - `pr_recovery.rs`: PR recovery startup scan and workflow/task candidate
     collection.
   - `system_recovery.rs`: system task recovery startup flow.
   - `checkpoint_recovery.rs`: checkpoint recovery startup flow.
   - `orphan_pending_recovery.rs`: orphan pending task recovery startup flow.
3. Preserve behavior while fixing module boundaries.
   - Move imports with the code that needs them.
   - Use `pub(super)` only where sibling `http` modules need the item through
     `http::background`; keep implementation helpers private where possible.
   - Avoid renaming functions, changing log fields, changing retry intervals,
     or changing match behavior.
4. Update file-size policy.
   - Remove the old `**/harness-server/src/http/background.rs` U-16 exemption
     from `CLAUDE.md`, or reduce it to a thin-root ceiling if the policy format
     requires an explicit entry.
5. Verify the public surface and behavior.
   - Run workspace check to catch missed re-exports and visibility regressions.
   - Run harness-server tests for covered behavior.
   - Run fmt, clippy, and SpecRail checks.

## Data Flow

No data flow changes are planned. Existing background loops continue to read and
mutate task store state, workflow runtime state, command records, GitHub PR
snapshots, runtime profile manifests, leases, and recovery candidates exactly as
before. Only the source-file layout changes.

## Alternatives Considered

- Leave `background.rs` as-is. Rejected because it remains near the exemption
  ceiling and continues to force unrelated background domains into one context.
- Use `include!` fragments. Rejected because production modules should be
  navigable Rust modules with normal imports and visibility.
- Split and redesign worker boundaries at the same time. Rejected because GH1463
  requires a move-only refactor and behavior changes would make review harder.
- Move call sites to child modules. Rejected because the issue requires the
  public `http::background` surface to remain unchanged.

## Risks

- Visibility can break sibling module imports. Mitigate with
  `cargo check --workspace --all-targets`.
- Shared helper placement can accidentally broaden access. Mitigate by using
  private helpers unless an existing `pub(super)` consumer requires re-export.
- Mechanical movement can hide behavior edits. Mitigate by keeping function
  names and bodies unchanged except for imports and visibility, then reviewing
  the diff as a move.
- File-size goals can fail if concern groups are too coarse. Mitigate by
  splitting large groups before PR readiness.

## Test Plan

- [ ] Run `cargo check --workspace --all-targets`.
- [ ] Run `cargo test -p harness-server`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1463`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `wc -l crates/harness-server/src/http/background.rs crates/harness-server/src/http/background/*.rs`.

## Rollback Plan

Revert the implementation commit. Because the implementation should only move
code and update `CLAUDE.md`, rollback restores the previous file layout without
a migration, data repair, or runtime configuration change.
