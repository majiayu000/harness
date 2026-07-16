# Tech Spec

## Linked Issue

GH-1639

## Product Spec

`specs/GH1639/product.md`

## Codebase Context

Verified at `origin/main` commit `a9110fda`.

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Claude test module | `crates/harness-agents/src/claude_tests.rs` | 928 lines; registered as `claude::tests`. | Exceeds the 800-line hard ceiling. |
| Lifecycle fixtures | `claude_tests.rs:761-928` | Three adjacent tests cover root exit, receiver-drop cancellation, and timeout/drop descendant cleanup. | Cohesive extraction boundary. |
| Parent helpers | `claude_tests.rs:13-390` | Provides `wait_for_path`, `write_executable_script`, imports, and environment guards. | Child test modules can reuse private parent names through `use super::*`. |
| Codex tests | `crates/harness-agents/src/codex_tests.rs` | 772 lines. | Below the ceiling and out of scope. |
| Module registration | `crates/harness-agents/src/claude.rs:509-512` | Uses a path override for the top-level test module. | Remains unchanged; nesting occurs inside `claude_tests.rs`. |

## Proposed Design

### 1. Extract one focused child module

Create `crates/harness-agents/src/claude_tests/lifecycle.rs`. Move the final
three lifecycle fixtures into it and add `use super::*;` so they reuse the
parent test module's private helpers and imported names.

Register it at the end of `claude_tests.rs` with:

```rust
#[path = "claude_tests/lifecycle.rs"]
mod lifecycle;
```

The explicit path is resolved consistently with the existing path-based parent
module and makes the file location unambiguous.

### 2. Preserve behavior exactly

Do not alter any moved fixture body. In particular, preserve:

- shell scripts and marker paths;
- 5-, 10-, 2-, and 1.5-second timing values;
- startup and descendant observation;
- receiver-drop and task-abort expectations;
- final assertions that descendants cannot mutate the workspace.

Only module nesting changes the fully qualified private test names.

### 3. Enforce the boundary

Verify both files with `wc -l`. Use a move-aware diff and direct source review
to prove the extracted block is unchanged. Do not touch `claude.rs`, production
cleanup, Codex tests, manifests, hooks, or workflows.

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001/B-002 line ceilings | parent and child test files | `wc -l` |
| B-003/B-005 module/helper reuse | parent registration and child import | compile plus source review |
| B-004 behavior preservation | extracted fixture block | move-aware diff and targeted tests |
| B-006 exact scope | repository diff | file list and manifest/hook diff checks |
| B-007 test readiness | `harness-agents` | targeted lifecycle filters, full package, full Clippy |
| B-008 GH-1637 handoff | resulting module layout | VibeGuard-accepted follow-up patch |

## Risks

- Module path resolution: the parent already uses a path override. Use an
  explicit child path and prove it with package compilation.
- Import drift: child tests need parent-private helpers and types. Use
  `use super::*` rather than duplicating helpers.
- False behavioral change: moving code can obscure edits. Limit the diff to
  deletion/addition of the same block plus module registration.
- Test filter changes: private names gain `lifecycle`; document and use the new
  names in targeted verification.

## Alternatives Considered

1. Bypass VibeGuard. Rejected: violates the hard ceiling and user constraints.
2. Add GH-1637 guards on compressed lines. Rejected: formatting restores line
   growth and evades rather than fixes the maintainability defect.
3. Split all Claude tests by category. Rejected: broader than needed for the
   prerequisite and higher move risk.
4. Move helpers with the fixtures. Rejected: provider/execution tests still use
   them, causing duplication or a larger reorganization.

## Test Plan

- [ ] `wc -l crates/harness-agents/src/claude_tests.rs crates/harness-agents/src/claude_tests/lifecycle.rs`.
- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo test -p harness-agents claude::tests::lifecycle -- --nocapture`.
- [ ] `cargo test -p harness-agents --lib`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `python3 checks/check_workflow.py --repo .`.
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1639`.
- [ ] Exact diff review confirming no production, assertion, timeout, script,
      dependency, hook, or workflow change.

## Rollback Plan

Revert the implementation PR. No runtime or data rollback is required; the
928-line blocker and GH-1637 implementation block would return.
