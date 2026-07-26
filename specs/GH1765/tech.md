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
| `target/` bloat | Reduced, unguarded | ~18 GB total (was 176 GB); six historical universes still present: `cargo-check` 866 MB, `cargo-build-main` 141 MB, `cargo-build` 138 MB, `cargo-test` 15 MB, `cargo-check-warnings` 9.7 MB, `cargo-test-local-fresh` 7.5 MB |

### Remaining defects this spec addresses

1. **Duplicated lint policy.** Four call sites still append `-- -D warnings`
   to clippy even though the manifest owns the policy:
   - `.githooks/pre-commit:74` — `cargo clippy $scope --all-targets -- -D warnings`
   - `.githooks/pre-push:33` — `cargo clippy --workspace --all-targets -- -D warnings`
   - `.github/workflows/ci.yml:124` — `cargo clippy --workspace --all-targets -- -D warnings`
   - `Makefile:12` — same invocation
   Trailing lint args affect only primary packages, so dependency
   fingerprints no longer flip; the cost is policy drift, not rebuilds.
2. **No target GC.** Nothing bounds `target/` size or removes dead
   universes; the 176 GB state can silently regrow.
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

### D2 — Target GC sweep

New script `scripts/target-gc.sh` (bash, no new dependencies), invoked
manually and optionally from a scheduled maintenance entry point:

- **Sanctioned universe list** — a shell array at the top of the script,
  mirrored in `CLAUDE.md`: `debug`, `release`, `cargo-check`, `cargo-test`,
  `cargo-clippy`, `package`, `tmp`.
- **Unsanctioned universes** — any other first-level directory under
  `target/` is a removal candidate in full.
- **Stale artifacts** — within sanctioned universes, files with mtime older
  than `TARGET_GC_MAX_AGE_DAYS` (default 30) are candidates.
- **Active-build safety** — a universe containing a live cargo lock
  (`.cargo-lock` held; detected via a non-blocking `flock` probe, or
  skipped wholesale if `flock` is unavailable on the platform) is skipped
  and reported as skipped.
- **Modes** — default is dry-run: print candidate paths and aggregate size,
  delete nothing. `--delete` performs removal and prints a summary of
  reclaimed bytes. Exit code 0 in both modes unless an I/O error occurs.
- **Scope guard** — the script refuses to operate unless
  `$(git rev-parse --show-toplevel)/target` is the resolved target root;
  it never follows symlinks out of it.

### D3 — CI regression guard

New job step in `.github/workflows/ci.yml` (in the existing lint job, before
clippy; runs on every PR, exempt from path filtering) executing
`scripts/check-build-posture.sh`:

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
the GC thresholds, and a pointer to `scripts/target-gc.sh`.

## Migration Order

1. Land D1 + D3 in one PR (guard immediately protects the consolidated
   state; guard check 5 makes D1 self-enforcing).
2. Land D2 + D4 in a follow-up PR; run one dry-run cycle on the primary dev
   machine; then enable `--delete` in the maintenance entry point.

## Validation

- `bash scripts/check-build-posture.sh` → exit 0 on clean tree.
- Seed each violation class in a scratch branch (re-add `RUSTFLAGS` to
  pre-commit; drop `[lints]` from one crate; drop the profile line; re-add
  `-- -D warnings`) → guard exits 1 naming the invariant, one class per run.
- `touch -t` fixture artifacts under a scratch `target/` layout →
  `scripts/target-gc.sh` dry-run lists exactly the stale + unsanctioned
  candidates; `--delete` removes exactly them.
- `cargo clippy --workspace --all-targets` with a seeded
  `let unused = 1;` fails, proving manifest-only enforcement (B-002).
- Standard gates: `cargo fmt --all -- --check`; full CI on the PR.

## Risks / Alternatives

- **R1:** Guard greps are brittle to formatting. Mitigation: assert on
  stable single-line tokens; keep the script trivial enough to fix inline.
- **R2:** GC deletes something a developer wanted. Mitigation: dry-run
  default, age gating, sanctioned-list skip, active-lock skip, and printed
  candidate list before `--delete`.
- **Alt considered:** adopting `cargo-sweep`/`cargo-cache` for D2. Rejected
  for now — adds a tool dependency for what a 60-line script covers; the
  guard (D3) is the part with durable value.
- **Alt considered:** removing per-universe target dirs entirely now that
  the flag universes are unified. Rejected — concurrent local cargo
  commands still contend on one build lock; isolation by command class
  remains the documented workaround.
