# Product Spec

## Linked Issue

GH-1460

## User Problem

Harness developer target directories can grow without bound across normal
workspace builds and parallel `CARGO_TARGET_DIR` universes. The reported primary
machine had `target/` at 176 GB, including large `deps`, `incremental`, and
auxiliary `target/cargo-*` directories. This consumes disk and makes workspace
checks spend substantial time on filesystem metadata for stale artifacts.

Developers need a bounded, manual cleanup tool that makes the size of each
target universe visible, removes stale build artifacts safely, and can be run
repeatedly without destructive surprises.

## Goals

- Provide a manual `scripts/gc-target.sh` cleanup command for `target/` and
  auxiliary `target/cargo-*` directories.
- Prefer `cargo sweep --time <days>` when the optional `cargo-sweep` tool is
  installed.
- Provide a deterministic fallback that removes stale files or directories by
  mtime when `cargo-sweep` is unavailable.
- Print before/after per-universe size reports and a clear freed-space summary.
- Add a `make gc-target` alias.
- Document usage, default retention, cadence, and safety caveats in
  `CONTRIBUTING.md`.

## Non-Goals

- Automatic scheduling through launchd, cron, CI, or background runtime tasks.
- Performing or recording the one-time cleanup of any specific developer
  machine's existing backlog.
- Changing Cargo profile settings or lint policy; those were handled by
  GH-1458 and GH-1459.
- Touching the `harness-gc` crate, which owns agent workspace cleanup and is a
  different domain.
- Deleting source files, Cargo manifests, lockfiles, or any path outside
  repository-local target directories.

## User-Visible Behavior

A developer can run the target GC script from the repository root. By default it
keeps artifacts newer than 14 days and considers the repository-local `target/`
directory plus auxiliary `target/cargo-*` directories. The script prints a size
table before cleanup, performs either the `cargo-sweep` path or fallback mtime
cleanup, and prints the size table again with bytes freed.

If `target/` does not exist, the command exits successfully with a clear no-op
message. If the command is run twice with the same retention window, the second
run should report no additional cleanup beyond any files that aged into the
window between runs.

The documentation must warn that running target GC while a Cargo build is
active can force rebuilds or conflict with Cargo's build directory activity.
The tool remains manual so the operator controls timing.

## Acceptance Criteria

- [ ] `scripts/gc-target.sh` exists, is executable, and supports a default
      retention window of 14 days.
- [ ] The script accepts a configurable retention window without requiring
      source edits.
- [ ] The script reports per-universe sizes before and after cleanup.
- [ ] When `cargo sweep` is installed, the script prefers `cargo sweep --time`
      or the equivalent installed CLI for supported target directories.
- [ ] When `cargo sweep` is not installed, the script falls back to bounded
      mtime-based cleanup inside `target/*/{deps,incremental,build}` and
      equivalent auxiliary `target/cargo-*` universes.
- [ ] Running the script with no `target/` directory succeeds as a no-op.
- [ ] Running the script twice in a row is idempotent; the second run reports
      no additional removals when no artifacts aged into scope.
- [ ] `make gc-target` runs the script.
- [ ] `CONTRIBUTING.md` documents usage, recommended cadence, retention, and
      the caveat about avoiding active Cargo builds.
- [ ] The script is `shellcheck` clean.

## Edge Cases

- `target/` exists but some expected child directories do not; the script should
  skip missing paths without error.
- File names may include spaces or unusual characters; cleanup should avoid
  unsafe word splitting.
- Running from a subdirectory should either resolve the repository root or fail
  with an explicit message.
- An artifact can disappear between size calculation and deletion; the script
  should tolerate that race and continue.
- If `cargo-sweep` fails for a target universe, the script should surface the
  failure instead of silently reporting success.

## Rollout Notes

This is a manual developer tooling change. The implementation PR should not
claim to have cleaned any operator machine. It should verify behavior against a
temporary target fixture and document real local disk measurements only as
environment-specific evidence.
