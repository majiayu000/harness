# Product Spec

## Linked Issue

GH-1765

## User Problem

Agent-driven iteration on this repository was throttled by the build system:
conflicting compile-flag universes, full debuginfo, per-commit full-workspace
gates, and a 176 GB `target/` directory that made every build
filesystem-bound. Most of that has since been remediated (manifest lints,
`line-tables-only` debuginfo, staged-scope pre-commit, target cleanup to
~18 GB), but the fixes are unguarded: lint policy is still duplicated in four
call sites, nothing prevents `target/` bloat from regrowing, the parallel
target-directory scheme is convention-only, and no automated check stops a
future PR from silently reintroducing the two-universe split.

Operators and agents need the build-performance posture to be owned by
configuration and enforced by automation, not by institutional memory.

## Goals

- Make the manifest lint table the single authority for warning policy;
  remove the duplicated `-D warnings` invocation arguments.
- Bound `target/` growth with an automated, age-gated garbage-collection
  sweep over an explicit list of sanctioned target-directory universes.
- Add a CI regression guard that fails when the unified-universe posture is
  violated (reintroduced `RUSTFLAGS`, missing manifest lints, missing
  per-crate `[lints]` table, altered dev-profile debuginfo).
- Canonicalize the sanctioned `CARGO_TARGET_DIR` names used for concurrent
  local commands.

## Non-Goals

- No dependency version changes of any kind.
- No `Cargo.toml` workspace version bump (release-time only).
- No weakening of CI semantics: the same lint set remains enforced with the
  same `deny` severity; only its point of definition is consolidated.
- No changes to which tests run in pre-commit, pre-push, or CI.
- No adoption of sccache, alternative linkers, or build caching services.
- No splitting of the remaining ≥ 1000-line source files.
- No changes to Postgres schema cleanup (tracked separately).

## User-Visible Behavior

1. **B-001:** Warning policy is defined exactly once, in the workspace
   manifest lint table, and inherited by every crate. Hook, Makefile, and CI
   clippy invocations carry no trailing lint-flag arguments.
2. **B-002:** A commit or CI run that violates the lint policy fails
   identically before and after this change; consolidating the definition
   point changes no pass/fail outcome.
3. **B-003:** The sanctioned target-directory universes are explicitly
   enumerated in repository documentation. Local tooling that isolates
   concurrent cargo commands uses only sanctioned names.
4. **B-004:** A garbage-collection sweep removes artifacts in unsanctioned
   target universes and artifacts older than a configured age threshold, and
   reports what it removed. It never deletes outside `target/`.
5. **B-005:** The sweep supports a dry-run mode that reports candidates
   without deleting, and dry-run is the default when invoked manually.
6. **B-006:** CI includes a guard step that fails when any of the following
   holds: `RUSTFLAGS` appears in `.githooks/`, workflow files, or `Makefile`;
   the workspace manifest lacks the lint table; any workspace crate manifest
   lacks `[lints] workspace = true`; the dev-profile debuginfo settings are
   removed.
7. **B-007:** The guard's failure output names the violated invariant and the
   offending file so an agent can repair it without archaeology.
8. **B-008:** All existing build entry points (`cargo check`, hook scripts,
   `make lint`, CI jobs) continue to work unchanged from a contributor's
   perspective.

## Acceptance Criteria

- [ ] `grep -rn "RUSTFLAGS" .githooks .github/workflows Makefile scripts`
      returns no matches, and the four former `-- -D warnings` call sites
      (`.githooks/pre-commit`, `.githooks/pre-push`, `.github/workflows/ci.yml`,
      `Makefile`) carry no trailing lint arguments.
- [ ] A deliberately introduced warning fails pre-commit clippy, pre-push
      clippy, and the CI clippy job, proving B-002.
- [ ] The GC sweep, run in dry-run mode against a fixture target layout
      containing one sanctioned and one unsanctioned universe plus stale
      artifacts, reports exactly the unsanctioned universe and stale
      artifacts as candidates and deletes nothing.
- [ ] The GC sweep in destructive mode removes exactly the reported
      candidates and leaves sanctioned, fresh artifacts intact.
- [ ] The CI guard fails on each seeded violation class from B-006 (one test
      per class) and passes on the clean tree.
- [ ] Documentation lists the sanctioned target-directory names and the GC
      thresholds.
- [ ] No `Cargo.lock` changes and no workspace version change in the
      implementing PR.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | GC sweep on a missing or empty `target/` is a no-op success (B-004). Guard on a tree with no crates fails loudly on the missing lint table (B-006). |
| Error and failure paths | Covered by B-006/B-007: guard failures are named and file-scoped. Sweep I/O errors abort the sweep without partial silent deletion beyond already-reported items. |
| Authorization / permission | N/A. Local filesystem and CI-internal checks only; no privilege changes. |
| Concurrency / race / ordering | Sweep must tolerate a concurrent cargo build holding locks: skip locked/active universes rather than fail (B-004). |
| Retry / repetition / idempotency | Sweep and guard are idempotent; re-running after a clean pass reports zero candidates / zero violations. |
| Illegal state transitions | N/A — no stateful lifecycle is introduced. |
| Compatibility / migration | Covered by B-002/B-008: no pass/fail outcome changes; no contributor-visible workflow changes. |
| Degradation / fallback | Guard step must not be skippable by path-filtering: it runs on every PR (cheap grep-level assertions). |
| Evidence and audit integrity | Sweep reports what it removed (B-004); guard names the violated invariant (B-007). |
| Cancellation / interruption / partial completion | An interrupted sweep leaves remaining artifacts for the next run; no tombstone state. |

## Edge Cases

- A contributor adds a new crate without `[lints] workspace = true` — the
  guard must catch it on the introducing PR.
- A new sanctioned universe is legitimately needed — adding it requires
  updating the documented list, which the sweep consumes.
- `target/` contains a universe currently in use by a running build — the
  sweep skips it.
- The dev profile is edited to `debug = true` for a local debugging session
  and accidentally committed — the guard fails the PR.
- A hook script is rewritten and someone re-adds `RUSTFLAGS=-Dwarnings` out
  of habit — the guard fails the PR.

## Rollout Notes

No migration or feature flag required. The lint-argument removal and the CI
guard land together so the guard immediately protects the consolidated
state. The GC sweep ships with dry-run default; destructive mode is enabled
after one observed clean dry-run cycle. Reverting the change restores the
duplicated flags and removes the guard, with no data impact.
