# Tech Spec

## Linked Issue

GH-1464

## Product Spec

See `specs/GH1464/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Commit hook | `.githooks/pre-commit` | Runs fmt, staged-scope clippy, and staged-scope lib tests. Unsets git environment variables before cargo commands. | The issue asks to make commit-time validation fast by removing tests from pre-commit. |
| Push hook | `.githooks/pre-push` | Does not exist. | The full local gate needs a new pre-push entrypoint. |
| Hook path setup | `build.rs`, `Makefile` | Configures `core.hooksPath` to `.githooks` when the pre-commit hook exists. | The new pre-push hook should work through the same hooks path without extra setup. |
| Agent docs | `CLAUDE.md`, `AGENTS.md` | Describe pre-commit as running fmt, clippy, and tests. | These instructions will become stale after the split. |
| Contributor docs | `CONTRIBUTING.md` | Describes generic test expectations but not the local hook split. | Contributors need the new local workflow documented. |

## Proposed Design

1. Keep the staged-scope derivation in `.githooks/pre-commit`.
   - Continue using `git diff --cached --name-only`.
   - Continue falling back to `--workspace` for ambiguous Rust or workspace
     files.
   - Keep `harness-core` dependent expansion because that mirrors the existing
     local and CI assumptions.
2. Remove test execution from `.githooks/pre-commit`.
   - Keep fmt first.
   - Keep clippy second.
   - End with a message that the fast pre-commit checks passed.
3. Add `.githooks/pre-push`.
   - Use `#!/usr/bin/env bash` and `set -euo pipefail`.
   - Unset `GIT_INDEX_FILE`, `GIT_DIR`, and `GIT_WORK_TREE` before running
     cargo commands.
   - Run `cargo clippy --workspace --all-targets -- -D warnings`.
   - Run `cargo test --workspace --exclude harness-server --lib` with
     `HARNESS_DATABASE_URL` unset for the command, so DB configuration does not
     leak into non-server tests that exercise process-global environment
     behavior.
   - When `HARNESS_DATABASE_URL` is set, run `cargo test -p harness-server
     --lib` after the non-server workspace pass.
   - When `HARNESS_DATABASE_URL` is unset, print an explicit skip message for
     `harness-server`, matching the CI convention that `harness-server` uses a
     dedicated DB profile.
4. Keep hook installation unchanged.
   - `build.rs` and `Makefile` can keep configuring `.githooks`; no special
     pre-push registration is needed once the hooks path is set.
5. Update docs.
   - `CLAUDE.md` and `AGENTS.md` should say pre-commit is fast fmt plus scoped
     clippy, and pre-push is full workspace clippy plus the split lib test
     gate.
   - `CONTRIBUTING.md` should tell contributors how to activate hooks and what
     runs at commit versus push time.

## Data Flow

No application data flow changes are planned. The only runtime effect is local
developer git hook execution.

## Alternatives Considered

- Keep tests in pre-commit but scope them more aggressively. Rejected because
  the issue explicitly moves full tests to pre-push and tests still add
  meaningful commit-time latency.
- Disable local full tests entirely and rely on CI. Rejected because the desired
  outcome keeps a local pre-push gate that blocks bad pushes.
- Add a hook manager dependency. Rejected because git's native hooks path is
  already configured and sufficient.
- Move full `cargo test --workspace --all-targets` to pre-push. Rejected
  because the current local gate is `cargo test --workspace --lib`; widening
  scope belongs in a separate decision.

## Risks

- Pre-push can feel slower than the current push path. This is the intended
  safety trade-off; CI remains the remote backstop.
- Removing tests from pre-commit can allow broken intermediate commits. This is
  accepted by the issue and mitigated by pre-push plus CI.
- Docs-only staged changes can still fall back to full clippy if scope
  derivation treats all non-crate files as ambiguous. Mitigate by making
  docs-only paths no-op for cargo scope while preserving full fallback for
  workspace-level Rust or cargo files.
- High-context docs can drift if only one instruction file is updated. Mitigate
  by updating `CLAUDE.md`, `AGENTS.md`, and `CONTRIBUTING.md` together.

## Test Plan

- [ ] Run `bash -n .githooks/pre-commit .githooks/pre-push`.
- [ ] Run a docs-only pre-commit simulation:
      `git add CONTRIBUTING.md && .githooks/pre-commit`, then unstage if
      needed.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `.githooks/pre-push` with local environment defaults.
- [ ] Run `.githooks/pre-push` with `HARNESS_DATABASE_URL` configured for a
      local test database.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1464`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.

## Rollback Plan

Revert the implementation commit. That restores the previous single pre-commit
gate and removes the pre-push hook without migrations, runtime state changes, or
data cleanup.
