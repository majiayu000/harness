# Task Plan

## Linked Issue

GH-1464

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1464-T001` Owner: `hook-scope` | Dependencies: none | Done when: `.githooks/pre-commit` keeps fmt plus staged-scope clippy and does not run Rust tests | Verify: inspect `.githooks/pre-commit` and run `bash -n .githooks/pre-commit`
- [ ] `SP1464-T002` Owner: `push-gate` | Dependencies: `SP1464-T001` | Done when: executable `.githooks/pre-push` runs full workspace clippy, non-server workspace lib tests without DB env leakage, and `harness-server` lib tests only when `HARNESS_DATABASE_URL` is set | Verify: `bash -n .githooks/pre-push` and `.githooks/pre-push`
- [ ] `SP1464-T003` Owner: `docs-only-fast-path` | Dependencies: `SP1464-T001` | Done when: docs-only staged changes do not cause pre-commit to run unrelated cargo clippy or tests | Verify: docs-only pre-commit simulation
- [ ] `SP1464-T004` Owner: `docs` | Dependencies: `SP1464-T001`, `SP1464-T002` | Done when: `CLAUDE.md`, `AGENTS.md`, and `CONTRIBUTING.md` describe fast pre-commit and full pre-push behavior | Verify: `rg -n "pre-commit|pre-push|tests" CLAUDE.md AGENTS.md CONTRIBUTING.md`
- [ ] `SP1464-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: syntax, docs-only pre-commit, both default and DB-configured pre-push paths, formatting, clippy, and SpecRail checks pass | Verify: `bash -n .githooks/pre-commit .githooks/pre-push`, docs-only pre-commit simulation, `.githooks/pre-push`, `HARNESS_DATABASE_URL=<test-db> .githooks/pre-push`, `cargo fmt --all -- --check`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1464`, and `python3 checks/check_workflow.py --repo .`

## Parallelization

Keep this implementation serial. The same hook files and high-context docs are
shared surfaces, so parallel writable lanes would create avoidable conflicts.

## Verification

- `bash -n .githooks/pre-commit .githooks/pre-push`
- docs-only pre-commit simulation
- `.githooks/pre-push`
- `HARNESS_DATABASE_URL=<test-db> .githooks/pre-push`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1464`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Use `Refs #1464` for the spec PR. The implementation PR should use
`Closes #1464` only after hook executable bits, docs, and verification are all
complete.
