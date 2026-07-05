# Tech Spec

## Linked Issue

GH-1460

## Product Spec

See `specs/GH1460/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Scripts | `scripts/` | The repo has operational shell scripts but no target-dir cleanup script. | New tooling belongs under `scripts/` and should follow existing shell style. |
| Make aliases | `Makefile` | Existing aliases cover setup, check, lint, and test. | `make gc-target` should be a thin wrapper around the script. |
| Contributor docs | `CONTRIBUTING.md` | Development setup describes build and test commands but no target-dir maintenance. | This is the canonical place for manual cadence and caveats. |
| Build outputs | `target/`, `target/cargo-*` | Build artifacts can accumulate across default and auxiliary target universes. | These are the only directories in scope for deletion. |
| Cargo profiles | `Cargo.toml` | GH-1458/GH-1459 already moved warning policy and dev debuginfo. | GH-1460 should not modify profile or lint settings. |
| Agent GC | `crates/harness-gc/` | Handles agent workspace cleanup. | It is intentionally unrelated to Cargo target cleanup. |

## Proposed Design

1. Add `scripts/gc-target.sh`.
   - Use `set -euo pipefail`.
   - Resolve the repository root from the script path or `git rev-parse
     --show-toplevel`.
   - Default retention to 14 days.
   - Support a documented flag such as `--days <N>` and `--dry-run`.
   - Reject invalid or negative retention values with a clear error.
2. Discover target universes.
   - Include the root `target/` directory when present.
   - Include direct child directories matching `target/cargo-*`.
   - Keep the universe list repository-local; do not inspect global Cargo
     caches or sibling repositories.
3. Report sizes.
   - Print `du -sh` for each discovered universe before cleanup.
   - Print the same report after cleanup.
   - Compute total bytes before and after using a stable command available on
     macOS and Linux, with a fallback if exact byte accounting is unavailable.
4. Prefer `cargo sweep`.
   - If `cargo sweep` is available, run it for each target universe with the
     configured retention window.
   - Pass the target directory explicitly where the installed CLI supports it;
     otherwise run from the repository root for the default `target/` and use
     the fallback for auxiliary universes that cannot be addressed safely.
5. Fallback cleanup.
   - When `cargo sweep` is not available, use `find` against bounded artifact
     subtrees: `deps`, `incremental`, and `build` under each universe.
   - Remove only paths older than the retention window.
   - Avoid following symlinks.
   - Support `--dry-run` by reporting what would be removed.
6. Wire `make gc-target`.
   - Add a phony target that invokes `scripts/gc-target.sh`.
7. Document usage in `CONTRIBUTING.md`.
   - Include default and custom retention examples.
   - Recommend running manually when disk grows or before large branch changes.
   - Warn not to run during active Cargo builds.

## Data Flow

The operator invokes `make gc-target` or `scripts/gc-target.sh`. The script
parses retention options, discovers repository-local target universes, captures
before sizes, executes either `cargo-sweep` or fallback cleanup for each
universe, captures after sizes, and prints the freed-space summary. No
application data, databases, remote services, or Harness runtime state are read
or modified.

## Alternatives Considered

- Add an automatic scheduled cleanup. Rejected because GH-1460 explicitly leaves
  cadence to the owner and requires manual safety.
- Put this in `harness-gc`. Rejected because `harness-gc` is for agent
  workspace cleanup, not Cargo build artifact maintenance.
- Delete all of `target/` unconditionally. Rejected because bounded stale
  cleanup is safer and keeps recent incremental artifacts.
- Require `cargo-sweep`. Rejected because the script must remain useful on a
  fresh developer machine without additional tools.

## Risks

- Security: shell path handling must avoid unsafe word splitting and must not
  delete outside repository-local target directories.
- Compatibility: `find`, `stat`, and `du` flags differ between macOS and Linux;
  the implementation should use portable forms or guarded helpers.
- Performance: size calculation over very large target directories can be slow,
  but it is part of the explicit manual maintenance command.
- Maintenance: `cargo-sweep` CLI flags may differ by version; the script should
  keep fallback cleanup deterministic and visible.

## Test Plan

- [ ] Script unit/fixture test: create a temporary target fixture with old and
      fresh files, run the fallback path, and assert only stale artifacts are
      removed.
- [ ] Idempotence test: run the script twice against the same fixture and assert
      the second run removes nothing.
- [ ] Fresh clone test: run the script when `target/` is absent and assert a
      successful no-op.
- [ ] Dry-run test: assert `--dry-run` reports candidates without deleting
      them.
- [ ] Shell lint: `shellcheck scripts/gc-target.sh` passes when shellcheck is
      installed; otherwise record that shellcheck is unavailable locally and
      rely on CI if configured.
- [ ] Repository checks: `cargo fmt --all -- --check`,
      `cargo clippy --workspace --all-targets -- -D warnings`, and SpecRail
      workflow checks pass.

## Rollback Plan

Remove `scripts/gc-target.sh`, the `make gc-target` alias, and the
`CONTRIBUTING.md` documentation. Because this is manual developer tooling, no
runtime state or persisted data rollback is required.
