# Tech Spec

## Linked Issue

GH-1765

## Current State

### Already landed (verified 2026-07-26)

| Concern | State | Evidence |
| --- | --- | --- |
| Lint policy in manifest | Landed | `Cargo.toml:26` `[workspace.lints.rust] warnings = "deny"`; all 13 `crates/*/Cargo.toml` carry `[lints] workspace = true` |
| `RUSTFLAGS` universes | Eliminated | zero matches for `RUSTFLAGS` under `.githooks/`, `.github/workflows/`, `Makefile`, `scripts/` |
| Dev debuginfo | Landed | `Cargo.toml:113-117`: `[profile.dev] debug = "line-tables-only"`; `[profile.dev.package."*"] debug = false` |
| Pre-commit scope | Landed | `.githooks/pre-commit` derives `-p` package scope from staged files; docs/specs-only commits skip clippy; no tests in pre-commit |
| `target/` bloat | Reduced; manual GC tooling exists | ~18 GB total (was 176 GB); six historical universes still present: `cargo-check` 866 MB, `cargo-build-main` 141 MB, `cargo-build` 138 MB, `cargo-test` 15 MB, `cargo-check-warnings` 9.7 MB, `cargo-test-local-fresh` 7.5 MB. `scripts/gc-target.sh` (PR #1545): age-based cleanup (`--days`, default 14), `--dry-run` opt-in, prefers `cargo sweep`, sweeps `target/` + all `target/cargo-*`, exposed as `make gc-target` (`Makefile:17-18`), tested by `scripts/test_gc_target.py` |

### Remaining defects this spec addresses

1. **Duplicated lint policy.** Four call sites still append `-- -D warnings`
   to clippy even though the manifest owns the policy:
   - `.githooks/pre-commit:74` — `cargo clippy $scope --all-targets -- -D warnings`
   - `.githooks/pre-push:33` — `cargo clippy --workspace --all-targets -- -D warnings`
   - `.github/workflows/ci.yml:124` — `cargo clippy --workspace --all-targets -- -D warnings`
   - `Makefile:12` — same invocation
   Trailing lint args affect only primary packages, so dependency
   fingerprints no longer flip; the cost is policy drift, not rebuilds.
2. **Target GC is manual and incomplete.** `scripts/gc-target.sh` covers
   age-based stale-artifact cleanup, but it is manual-only, destructive by
   default (`--dry-run` is opt-in), removes no unsanctioned universe
   wholesale, has no sanctioned-universe list, does not skip universes with
   an active cargo build, and has no scheduled invocation — so the 176 GB
   state can silently regrow between manual runs.
3. **No regression guard.** No automated check protects the manifest-lints /
   profile / no-RUSTFLAGS posture.
4. **Universe list is convention-only.** `CLAUDE.md` sanctions
   `target/cargo-check`, `target/cargo-test`, `target/cargo-clippy`; the
   other three observed universes are drift.

## Design

### D1 — Single lint authority

Remove the trailing `-- -D warnings` (and any other trailing lint args) from
the four call sites. Resulting invocations:

```
cargo clippy $scope --all-targets        # pre-commit
cargo clippy --workspace --all-targets   # pre-push, ci.yml, Makefile
```

Enforcement path: `[workspace.lints.rust] warnings = "deny"` +
`[lints] workspace = true` in every crate. Clippy-specific lint groups can be
added later under `[workspace.lints.clippy]` without touching call sites.

No behavioral change: a warning in any workspace crate already fails
compilation under the manifest table.

### D2 — Extend `scripts/gc-target.sh`

The existing script stays the single GC entry point (no new script). It
already provides: `--days N` age windows (default 14), `--dry-run`,
`cargo sweep` preference with mtime fallback, enumeration of `target/` plus
every `target/cargo-*` universe, `make gc-target`, and Python tests
(`scripts/test_gc_target.py`). The residual delta to implement in place:

- **Sanctioned universe list** — a shell array at the top of the script,
  mirrored in `CLAUDE.md`: `debug`, `release`, `cargo-check`, `cargo-test`,
  `cargo-clippy`, `package`, `tmp`.
- **Unsanctioned universes** — any other first-level directory under
  `target/` becomes a removal candidate in full (today such universes are
  only swept for stale files, never retired).
- **Dry-run by default** — flip the current destructive default: with no
  flags the script prints candidates and deletes nothing; destructive mode
  requires an explicit `--delete`. `--dry-run` is retained as a no-op alias
  for compatibility. This is a deliberate behavior change (product B-005).
- **Active-build safety** — a universe holding a live cargo build lock
  (non-blocking `flock` probe on its lock file; skip the universe wholesale
  if `flock` is unavailable on the platform) is skipped and reported as
  skipped, replacing the current "do not run during builds" doc-comment
  honor system.
- **Scheduled invocation** — a new launchd/cron wrapper runs the script
  with `--delete` after the rollout gate in product Rollout Notes is met.
- **Scope guard** — the script keeps refusing to operate outside the
  resolved repo `target/` root and never follows symlinks out of it.
- **Tests** — extend `scripts/test_gc_target.py` to cover the new
  candidate classes, the flipped default, and lock-skip behavior.

### D3 — CI regression guard

New **standalone job** `build-posture` in `.github/workflows/ci.yml`. It must
be unconditional: no `if:` change-detection expression and no `needs:` on
`changed` or `web-build`, because the existing clippy job is path-filtered
(`if: needs.changed.outputs.rust == 'true' || needs.changed.outputs.ci ==
'true'`, `ci.yml:109`) and a PR touching only `.githooks/` or `Makefile` —
exactly the regression class B-006 targets — would skip a step placed there.
The job is a single checkout + `scripts/check-build-posture.sh` run
(seconds of runtime, so unconditional execution is cheap).

Aggregation into the required check: the `CI Result` job (`ci.yml:233`,
`if: always()`) adds `build-posture` to its `needs:` list and its
`results=(...)` array, so a posture violation fails `CI Result` — the
branch-protection-required status — even though `build-posture` itself is
not individually required.

The script asserts:

```
1. ! grep -rn "RUSTFLAGS" .githooks .github/workflows Makefile scripts
2. grep -q "^\[workspace.lints.rust\]" Cargo.toml
3. for m in crates/*/Cargo.toml: grep -q "^\[lints\]" $m && grep -q "workspace = true"
4. grep -q 'debug = "line-tables-only"' Cargo.toml   # [profile.dev]
5. ! grep -n '\-\- -D warnings' .githooks/* .github/workflows/ci.yml Makefile
```

Each failed assertion prints `posture violation: <invariant> in <file>` and
exits 1. The script is also runnable locally.

Note: check 1 must exclude `scripts/check-build-posture.sh` itself and
documentation files; implement with an explicit exclude list.

### D4 — Documentation

`CLAUDE.md` "Local Cargo Concurrency" section updated with: the sanctioned
universe list (single source shared with D2 via comment cross-reference),
the GC thresholds, and a pointer to `scripts/gc-target.sh`.

## Migration Order

1. Land D1 + D3 in one PR (guard immediately protects the consolidated
   state; guard check 5 makes D1 self-enforcing).
2. Land D2 + D4 in a follow-up PR; run one dry-run cycle on the primary dev
   machine; then enable `--delete` in the maintenance entry point.

## Product-to-Test Mapping

| Behavior | Validation step |
| --- | --- |
| B-001 | Grep assertions over the four call sites + guard check 5; manifest lint table grep |
| B-002 | Seeded-warning run fails pre-commit clippy, pre-push clippy, and CI clippy without trailing lint args |
| B-003 | Sanctioned list present in script array and `CLAUDE.md`; unsanctioned fixture universe classified as removal candidate |
| B-004 | `test_gc_target.py` fixture: destructive run removes exactly reported candidates, nothing outside `target/` |
| B-005 | `test_gc_target.py`: flag-less invocation deletes nothing; `--delete` required for removal |
| B-006 | One seeded-violation CI/guard-script test per invariant class |
| B-007 | Guard output asserts `posture violation: <invariant> in <file>` format |
| B-008 | Clean-tree runs of pre-commit, pre-push, `make lint`, and full CI pass unchanged |

## Rollback

Revert the implementing PR(s): call sites regain the redundant lint args,
`gc-target.sh` returns to destructive-default, and the `build-posture` job
disappears from `CI Result` aggregation. No data, schema, or state
migration is involved in either direction.

## Validation

- `bash scripts/check-build-posture.sh` → exit 0 on clean tree.
- Seed each violation class in a scratch branch (re-add `RUSTFLAGS` to
  pre-commit; drop `[lints]` from one crate; drop the profile line; re-add
  `-- -D warnings`) → guard exits 1 naming the invariant, one class per run.
- `touch -t` fixture artifacts under a scratch `target/` layout →
  flag-less `scripts/gc-target.sh` lists exactly the stale + unsanctioned
  candidates and deletes nothing; `--delete` removes exactly them
  (extended `scripts/test_gc_target.py`).
- `cargo clippy --workspace --all-targets` with a seeded
  `let unused = 1;` fails, proving manifest-only enforcement (B-002).
- Standard gates: `cargo fmt --all -- --check`; full CI on the PR.

## Risks / Alternatives

- **R1:** Guard greps are brittle to formatting. Mitigation: assert on
  stable single-line tokens; keep the script trivial enough to fix inline.
- **R2:** GC deletes something a developer wanted. Mitigation: dry-run
  default, age gating, sanctioned-list skip, active-lock skip, and printed
  candidate list before `--delete`.
- **Alt considered:** replacing `scripts/gc-target.sh` with a new script or
  a `cargo-cache` dependency. Rejected — the existing script already
  integrates `cargo sweep` opportunistically and has tests; extending it
  preserves the entry point (`make gc-target`) and its test suite. The
  guard (D3) is the part with durable value.
- **Alt considered:** removing per-universe target dirs entirely now that
  the flag universes are unified. Rejected — concurrent local cargo
  commands still contend on one build lock; isolation by command class
  remains the documented workaround.
