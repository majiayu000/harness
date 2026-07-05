# Product Spec

## Linked Issue

GH-1463

## User Problem

`crates/harness-server/src/http/background.rs` is still a 3,542-line
maintenance hotspot on current main and requires a 3,600-line U-16 exemption in
`CLAUDE.md`. Routine server background work forces agents and maintainers to
load unrelated runtime dispatch, PR feedback, worker, and recovery code in one
file. The file is also a concentrated merge-conflict surface.

Maintainers need the file split into focused modules without changing route
behavior, task recovery behavior, runtime command dispatch behavior, or public
module paths.

## Goals

- Split `crates/harness-server/src/http/background.rs` into focused modules
  under `crates/harness-server/src/http/background/`.
- Keep `http::background` as the public module surface for existing callers by
  re-exporting the moved `pub` and `pub(super)` items from the root module.
- Keep the change mechanical: move code, fix imports and visibility, and avoid
  behavior, response-shape, or scheduling policy changes.
- Keep every resulting background module below about 1,200 lines.
- Update the `CLAUDE.md` U-16 exemption so the old 3,600-line
  `background.rs` allowance is removed or reduced to the new thin root.
- Prove server and workspace compilation still pass with the unchanged public
  surface.

## Non-Goals

- Changing route behavior, response payloads, scheduler timing, worker limits,
  runtime dispatch policy, auto-merge policy, or recovery semantics.
- Splitting other oversized files.
- Moving call sites outside `crates/harness-server/src/http/background*`.
- Adding new background workers, metrics, configuration, or tests beyond what
  is required to preserve compile coverage for the move.

## User-Visible Behavior

There is no runtime behavior change. Operators and API clients should observe
the same background worker, runtime command dispatch, PR feedback, and recovery
behavior. Developers and agents should see smaller focused modules instead of a
single oversized `background.rs` file.

## Acceptance Criteria

- [ ] `crates/harness-server/src/http/background.rs` becomes a thin module
      entrypoint that declares and re-exports focused child modules.
- [ ] Moved public items remain reachable through the existing
      `http::background` module path, so existing call sites do not move.
- [ ] No production behavior, response-shape, scheduling, runtime dispatch, or
      recovery logic is intentionally changed.
- [ ] No file in `crates/harness-server/src/http/background/` exceeds about
      1,200 lines.
- [ ] `CLAUDE.md` no longer grants
      `**/harness-server/src/http/background.rs` a 3,600-line exemption.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo test -p harness-server` passes with the existing local Postgres
      gating behavior.
- [ ] `cargo fmt --all -- --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes before PR
      merge readiness.

## Edge Cases

- Background startup recovery must keep current behavior for runtime-host leases,
  prompt orphan recovery, and task completion callbacks.
- Runtime command dispatch must keep current profile resolution, project-root
  resolution, command gate hashing, and outbox status transitions.
- PR feedback sweeping and auto-merge request recovery must keep current guard
  behavior and data extraction.
- Worker supervision must keep current lease, shutdown, concurrency, and
  external-owner checks.
- Existing `pub(super)` consumers in sibling `http` modules must continue to
  compile through the `http::background` root path.

## Rollout Notes

This is a mechanical maintainability refactor. The spec PR should use
`Refs #1463`. The implementation PR should use `Closes #1463` only after the
file-size target and verification commands pass.
