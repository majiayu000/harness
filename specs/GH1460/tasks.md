# Task Plan

## Linked Issue

GH-1460

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1460-T001` Owner: `target-gc-script` | Dependencies: none | Done when: `scripts/gc-target.sh` exists, is executable, parses `--days` and `--dry-run`, discovers repository-local target universes, reports before/after sizes, and refuses invalid retention values | Verify: `scripts/gc-target.sh --help` and fixture-based script tests
- [ ] `SP1460-T002` Owner: `cargo-sweep-path` | Dependencies: `SP1460-T001` | Done when: the script prefers `cargo sweep` when installed and surfaces any sweep failure instead of silently falling back to success | Verify: PATH shim test that records `cargo sweep` invocation
- [ ] `SP1460-T003` Owner: `fallback-cleanup` | Dependencies: `SP1460-T001` | Done when: the fallback path removes only stale artifacts under bounded `deps`, `incremental`, and `build` directories, skips missing paths, avoids symlink traversal, and supports dry-run | Verify: temporary fixture tests for stale, fresh, missing, and dry-run cases
- [ ] `SP1460-T004` Owner: `make-and-docs` | Dependencies: `SP1460-T001` | Done when: `make gc-target` invokes the script and `CONTRIBUTING.md` documents usage, cadence, retention, and active-build caveats | Verify: `make -n gc-target` plus documentation review
- [ ] `SP1460-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: shell lint, focused script tests, formatting, clippy, and SpecRail checks pass, with any unavailable optional local tooling explicitly recorded | Verify: `shellcheck scripts/gc-target.sh`, focused fixture tests, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1460`, and `python3 checks/check_workflow.py --repo .`

## Parallelization

`SP1460-T001` is the foundation. `SP1460-T002`, `SP1460-T003`, and
`SP1460-T004` can proceed after the script shape is established if file
ownership is kept disjoint. `SP1460-T005` is the final verification gate.

## Verification

- `scripts/gc-target.sh --help`
- focused fixture tests for no-target, stale cleanup, idempotence, dry-run, and
  cargo-sweep preference
- `make -n gc-target`
- `shellcheck scripts/gc-target.sh`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1460`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Do not close GH-1460 from a spec-only PR. Use `Refs #1460` for the spec PR and
reserve a closing keyword for the implementation PR that adds the script, make
alias, docs, and verification evidence.
