# Task Plan

## Linked Issue

GH-1459

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1459-T001` Owner: `workspace-profile` | Dependencies: none | Done when: root `Cargo.toml` sets `[profile.dev] debug = "line-tables-only"` and does not add unrelated release/profile changes | Verify: targeted manifest inspection
- [ ] `SP1459-T002` Owner: `dependency-debug-decision` | Dependencies: `SP1459-T001` | Done when: the implementation either adds `[profile.dev.package."*"] debug = false` with evidence or records why dependency debuginfo stripping was deferred | Verify: PR body decision note plus manifest inspection
- [ ] `SP1459-T003` Owner: `diagnostics-verification` | Dependencies: `SP1459-T001` | Done when: a temporary controlled panic or failing test proves file-line panic output still appears, and the temporary change is reverted before commit | Verify: captured command output or PR-body evidence
- [ ] `SP1459-T004` Owner: `workspace-tests` | Dependencies: `SP1459-T001` | Done when: workspace lib tests pass under the new profile, using existing Postgres-gated behavior where applicable | Verify: `cargo test --workspace --lib`
- [ ] `SP1459-T005` Owner: `size-evidence` | Dependencies: `SP1459-T001`, optional `SP1459-T002` | Done when: before/after `du -sh target/debug` evidence after a clean rebuild is recorded, or a limitation is explicitly documented | Verify: PR body evidence
- [ ] `SP1459-T006` Owner: `specrail` | Dependencies: all implementation tasks | Done when: SpecRail workflow checks pass and the PR body records whether GH-1458 landed in the same implementation PR or an adjacent PR | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1459 && python3 checks/check_workflow.py --repo .`

## Parallelization

`SP1459-T001` is the shared foundation. `SP1459-T002`, `SP1459-T003`, and
`SP1459-T004` can proceed after the profile change if file ownership and
temporary test edits stay isolated. `SP1459-T005` depends on the final profile
choice. `SP1459-T006` is the final gate.

## Verification

- targeted `Cargo.toml` inspection
- temporary controlled panic/backtrace check, reverted before commit
- `cargo test --workspace --lib`
- `du -sh target/debug` before and after clean rebuild, or documented limitation
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1459`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

GH-1459 is intentionally coordinated with GH-1458. A combined implementation PR
may close both issues only if it satisfies both spec packets. A spec-only PR
must use `Refs #1459` and must not close the issue.
