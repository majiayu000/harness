# Build Iteration Performance — Analysis Report

> Linked issue: GH-1765
> Original diagnosis: 2026-07-04 (read-only inspection + live measurement, M2 12-core / 96 GB, rustc 1.94)
> State partially re-verified: 2026-07-26 (spot checks of the items in §4; not a full re-measurement)
> Scope: why agent-driven iteration on this repository was slow, what has been
> remediated since, what remains, and how to keep it from regressing.

## 1. Problem statement

Harness evolves primarily through agent-driven iteration on its own codebase:
edit → check → commit → hook gates → push → CI. Every second of build latency
is multiplied by the per-step commit style agents use and by every concurrent
agent lane. In July 2026 that loop had degraded to the point where the build
system — not model latency — was the dominant cost of iteration.

Because build speed is a multiplier on every other improvement loop
(implementation, review, quality gates, evals), this is the highest-leverage,
lowest-risk performance surface in the project.

## 2. Measured facts (2026-07-04 baseline)

| # | Fact | Source |
|---|------|--------|
| F1 | `target/` totaled **176 GB**: `debug` 133 GB (deps 73 GB + incremental 58 GB) plus six parallel `CARGO_TARGET_DIR` universes (`cargo-test` 15 GB, `cargo-check` 7 GB, `cargo-test-local-fresh` 4.4 GB, `cargo-build-main` 3.4 GB, `cargo-build` 3.4 GB, `cargo-check-warnings` 1.5 GB) | `du -sh target/*` |
| F2 | Root `Cargo.toml` had **no `[profile]` section** → dev profile used default `debug = true` (full debuginfo); no `[lints]` table existed anywhere in the workspace | `Cargo.toml` at that commit |
| F3 | Three flag universes shared one `target/debug`: (a) daily plain `cargo check`, (b) pre-commit `RUSTFLAGS="-Dwarnings" cargo clippy --workspace --all-targets`, (c) pre-push/CI `RUSTFLAGS=-Dwarnings` as a global env | `.githooks/pre-commit`, `ci.yml` at that commit |
| F4 | Pre-commit ran on **every commit**: `cargo fmt --check` + full-workspace clippy `--all-targets` + `cargo test --workspace --lib` | `.githooks/pre-commit` at that commit |
| F5 | Workspace: 165k+ lines of Rust across 13 crates; 482 locked dependencies including compile-heavy sqlx, opentelemetry-otlp (tonic/prost), starlark, axum, reqwest | `wc -l`, `Cargo.lock` |
| F6 | **23 source files ≥ 1000 lines**; largest `runtime/tests.rs` at 6480 lines | `wc -l` sweep |
| F7 | Baseline `cargo check --workspace --all-targets` = **1m 41s wall; user 110.4 s, sys 95.7 s** | `/usr/bin/time -p` |
| F8 | `target/debug` fingerprints last touched ~3 weeks before the measurement session | `stat .fingerprint` |
| F9 | No sccache/mold/lld; no project or user `.cargo/config.toml` | filesystem check |
| F10 | 2600 test functions; `build.rs` shells out to `git config core.hooksPath` on every build | grep count; `build.rs` |

## 3. Inference chain

- **I1 (high confidence)** — [F1, F3, F8] The dominant iteration cost was
  flag-universe ping-pong. Changing `RUSTFLAGS` invalidates every Cargo
  fingerprint in a shared target directory. A typical agent loop (plain check →
  commit → `-Dwarnings` clippy → next plain check) forced a large rebuild on
  each flip; the 73 GB of dependency artifacts and 58 GB of incremental state
  were the two universes' artifacts accumulating in one directory.
- **I2 (high confidence)** — [F4, F7] Per-commit gating cost minutes,
  multiplied by per-step commits. Even the mild no-flip case measured 1m41s;
  a universe flip additionally rebuilt dependency crates, with full clippy and
  workspace lib tests on top.
- **I3 (medium-high confidence)** — [F7] `sys` time (95.7 s) nearly equaled
  `user` time (110.4 s); effective parallelism was ~2 of 12 cores.
  Compilation is normally user-CPU-bound; this profile indicates heavy
  filesystem metadata work — the bloated target directory itself taxed every
  build.
- **I4 (high confidence)** — [F2] Default `debug = true` inflated every
  artifact and link step, multiplied across all test binaries under
  `--all-targets`.
- **I5 (medium confidence)** — [F6] Oversized files burn agent context on
  every read/edit and concentrate merge conflicts across parallel agent lanes.
- **I6 (medium confidence)** — [F1] Six parallel `CARGO_TARGET_DIR` universes
  (a workaround for build-lock contention) each paid a cold build of all 482
  dependencies, multiplying disk and cold-start cost.

## 4. Remediation status (verified 2026-07-26)

Much of the original recommendation set has already landed:

| Item | Status | Evidence (current tree) |
|------|--------|-------------------------|
| R1 — unify the compile universe (manifest lints, no `RUSTFLAGS`) | **Done** | `Cargo.toml:26` `[workspace.lints.rust] warnings = "deny"`; all 13 crates carry `[lints] workspace = true`; `grep -r RUSTFLAGS .githooks .github/workflows Makefile scripts` → zero matches |
| R2 — cut debuginfo | **Done** | `Cargo.toml:113-117`: `[profile.dev] debug = "line-tables-only"`, `[profile.dev.package."*"] debug = false` |
| R3 — slim pre-commit | **Done** | `.githooks/pre-commit` now derives a staged-file package scope, skips clippy entirely for docs/specs-only commits, and no longer runs tests |
| R4 — GC `target/` | **Largely done; tooling exists, gaps remain** | `target/` now ~18 GB total (`debug` 16 GB, `cargo-check` 866 MB, `release` 619 MB, remainder < 300 MB each); fingerprints fresh (2026-07-23). `scripts/gc-target.sh` (merged 2026-07-05, PR #1545) provides age-based cleanup (`--days`, default 14) with `--dry-run`, prefers `cargo sweep`, sweeps `target/` plus all `target/cargo-*` universes, is exposed as `make gc-target`, and has Python tests (`scripts/test_gc_target.py`). Remaining gaps: manual-only invocation, destructive by default, no sanctioned-universe list, no removal of unsanctioned universes, no active-build lock skip |
| File splits | **Mostly done** | Files ≥ 1000 lines: 23 → **8** (largest now `harness-cli/src/commands.rs` at 1651) |
| R5 — residual PG schema cleanup | **Improved, backlog remains** | Orphan-schema reaper wired (historic ~538k → ~17k schemas at last measurement); backlog cleanup is a runtime-performance item tracked separately |

## 5. Remaining gaps

1. **Redundant `-D warnings` lint arguments.** Four sites still pass
   `-- -D warnings` to clippy (`.githooks/pre-commit:74`,
   `.githooks/pre-push:33`, `.github/workflows/ci.yml:124`, `Makefile:12`)
   even though the manifest lint table now enforces the same policy. Trailing
   lint args apply only to primary packages, so the dependency universe is no
   longer flipped — but the flags are now dead policy duplicated in four
   places, and any future edit to one site reintroduces drift between "what
   the manifest enforces" and "what hooks enforce".
2. **Target GC is manual and incomplete.** `scripts/gc-target.sh` already
   handles age-based cleanup of `target/` and every `target/cargo-*`
   universe, but it must be invoked by hand, deletes by default (dry-run is
   opt-in), has no notion of a sanctioned universe list (an unsanctioned
   universe is swept for stale files, never removed wholesale), does not
   skip universes with an active cargo build, and is not wired into any
   scheduled maintenance. The 176 GB state accumulated silently over months
   and taxed every build (I3); a manual opt-in script does not prevent
   recurrence.
3. **Parallel target-dir scheme is convention-only.** `CLAUDE.md` sanctions
   per-command target dirs (`target/cargo-check`, `target/cargo-test`,
   `target/cargo-clippy`), but historical drift produced six universes with
   ad-hoc names (`cargo-build-main`, `cargo-test-local-fresh`,
   `cargo-check-warnings`). Each nonstandard universe pays a cold dependency
   build.
4. **No regression guard.** Nothing detects the reintroduction of `RUSTFLAGS`
   into hooks/CI, a second lint universe, or a `[profile]` regression. All the
   July gains are one careless PR away from silently unwinding.
5. **No linker/cache assist.** No fast linker configuration and no
   compilation cache (F9 unchanged). On macOS the default linker is adequate;
   this is the lowest-priority residual.
6. **Eight files ≥ 1000 lines remain** (`commands.rs` 1651,
   `operator_monitor/tests.rs` 1595, `misc_routes.rs` 1120,
   `prompts/parsing.rs` 1064, `types.rs` 1027, plus three test files).
   Tracked by existing size-limit exemptions in `CLAUDE.md`; not re-scoped
   here.

## 6. Recommendations (priority order)

### R-A — Single lint authority (low risk, removes drift)

Delete the trailing `-- -D warnings` from the four call sites; the manifest
lint table is the single source of truth. Semantics are unchanged: the same
lint set is enforced, from one place.

- Risk: none for workspace crates (manifest lints already deny warnings).
- Alternative: keep the flags as belt-and-braces. Rejected — four copies of a
  policy that the manifest already owns is exactly the drift pattern that
  produced the original two-universe split.

### R-B — Extend `scripts/gc-target.sh` into a bounded, automated sweep

Keep the existing script as the single GC entry point and close the residual
delta: enumerate the sanctioned universe list (anything outside it is
removed wholesale, not just swept for stale files), flip the default to
dry-run with an explicit `--delete` for destructive mode, skip universes
holding an active cargo build lock, and wire a scheduled invocation so
hygiene is not manual-only. Surface current size in the existing ops/health
tooling rather than a new system.

- Risk: an over-eager sweep forces a cold rebuild. Mitigate with age gating
  and the dry-run default.
- Alternative: leave the script manual-only. Rejected — the 176 GB state is
  evidence that unowned manual hygiene does not happen.

### R-C — Regression guard in CI

A cheap CI step asserting: no `RUSTFLAGS` in `.githooks/`, workflow files, or
`Makefile`; `[workspace.lints.rust]` present; every crate manifest carries
`[lints] workspace = true`; `[profile.dev]` debuginfo setting intact.

- Risk: trivially low; pure grep-level assertions.
- Alternative: rely on review. Rejected — this class of regression is
  invisible in diffs unless the reviewer knows the history.

### R-D — Canonicalize the parallel-target scheme

Document the exact sanctioned `CARGO_TARGET_DIR` names and fold stragglers
into them; R-B's sweep enforces the list mechanically.

### R-E (optional) — Compilation cache

Evaluate sccache once R-A–R-D have landed and re-measure first; with a
unified universe and slim debuginfo the residual win may not justify the
operational surface.

## 7. Why this compounds

Every harness improvement loop terminates in a build: implement → check,
review-fix → check, quality gate → validation commands, eval case →
`verify_commands`. A 2× build speedup is a 2× speedup on the inner loop of
all of them, and cheaper iteration directly raises how much verification the
system can afford per change — which is the direction the rest of the
roadmap (server-verified completion, eval flywheel) pushes.
