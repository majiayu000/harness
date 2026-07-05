# Product Spec

## Linked Issue

GH-1464

## User Problem

The local `.githooks/pre-commit` hook still runs formatting, clippy, and Rust
tests on every commit. It already derives a staged cargo package scope, but the
test step is still paid at commit time. Harness agents commit often during one
task, so even scoped tests compound into slow iteration and make small commits
feel expensive.

Maintainers need the commit-time hook to stay quick while preserving the current
full local safety gate before code leaves the machine.

## Goals

- Keep `.githooks/pre-commit` focused on fast commit-time validation:
  `cargo fmt --all -- --check` plus clippy for the staged crate scope.
- Move the full gate to a new executable `.githooks/pre-push` hook:
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace --lib` with the existing local Postgres conditional.
- Preserve the existing `GIT_INDEX_FILE`, `GIT_DIR`, and `GIT_WORK_TREE`
  unsetting workaround for any hook that runs cargo commands.
- Keep `build.rs` hook-path auto-configuration working through
  `git config core.hooksPath .githooks`.
- Update contributor and agent-facing documentation so local hook expectations
  match the new split.

## Non-Goals

- Changing CI workflows or required GitHub status checks.
- Changing clippy warning policy, lint levels, or which warnings fail CI.
- Changing the staged crate scope derivation beyond what is needed to keep the
  pre-commit hook reliable.
- Adding new dependencies for shell linting or hook management.
- Optimizing Rust compile times outside the hook split.

## User-Visible Behavior

Developers and agents should see faster local commits for scoped source changes.
Pushes should run the full workspace clippy and lib test gate before the branch
is uploaded. A push containing a clippy warning or failing lib test should be
blocked locally before CI.

## Acceptance Criteria

- [ ] `.githooks/pre-commit` remains executable, uses bash with
      `set -euo pipefail`, runs formatting, and runs clippy for the staged
      cargo package scope.
- [ ] `.githooks/pre-commit` no longer runs Rust tests.
- [ ] `.githooks/pre-push` exists, is executable, uses bash with
      `set -euo pipefail`, unsets the leaking git environment variables, and
      runs the full workspace clippy gate.
- [ ] `.githooks/pre-push` runs non-server workspace lib tests without leaking
      `HARNESS_DATABASE_URL`, then runs `harness-server` lib tests only when
      `HARNESS_DATABASE_URL` is set.
- [ ] A docs-only staged change can complete the pre-commit hook without
      running clippy or tests for unrelated crates.
- [ ] `build.rs` continues to configure `core.hooksPath` for `.githooks`.
- [ ] `CLAUDE.md`, `AGENTS.md`, and `CONTRIBUTING.md` describe the new
      pre-commit/pre-push split without stale references to per-commit tests.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes before PR
      merge readiness.

## Edge Cases

- Workspace-level files such as `Cargo.toml`, `Cargo.lock`, or Rust files
  outside `crates/` should continue to fall back to a full clippy scope.
- Docs-only commits should not invoke unrelated Rust package checks in
  pre-commit.
- The pre-push hook must not break tests that create their own temporary git
  repositories.
- Machines without a local Postgres database should receive an explicit
  `harness-server` skip message and still run non-server workspace lib tests.

## Rollout Notes

Use `Refs #1464` for the spec PR. The implementation PR should use
`Closes #1464` only after both hooks are executable, local verification passes,
and the docs no longer describe the old per-commit test gate.
