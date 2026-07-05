# Task Plan

## Linked Issue

GH-1458

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1458-T001` Owner: `workspace-manifest` | Dependencies: none | Done when: the root `Cargo.toml` defines `[workspace.lints.rust] warnings = "deny"` and every workspace crate manifest opts into `[lints] workspace = true` | Verify: manifest grep or script over `crates/*/Cargo.toml`
- [ ] `SP1458-T002` Owner: `ci-hooks` | Dependencies: `SP1458-T001` | Done when: `.github/workflows/ci.yml`, `.github/workflows/ci-disabled-modules.yml`, and `.githooks/pre-commit` no longer use `RUSTFLAGS=-Dwarnings` for warning policy | Verify: targeted `rg -n "RUSTFLAGS.*Dwarnings|Dwarnings.*RUSTFLAGS" .github .githooks`
- [ ] `SP1458-T003` Owner: `active-docs` | Dependencies: `SP1458-T001` | Done when: active repo instructions and current developer commands describe no-env cargo checks plus clippy's `-- -D warnings` where needed | Verify: targeted `rg -n "RUSTFLAGS.*Dwarnings|Dwarnings.*RUSTFLAGS" AGENTS.md CLAUDE.md README.md scripts docs`
- [ ] `SP1458-T004` Owner: `verification` | Dependencies: `SP1458-T001`, `SP1458-T002`, `SP1458-T003`, GH-1459 implementation when combined | Done when: clean no-env workspace check and clippy pass, an intentional warning fails before being reverted, and repeat check/clippy timing or no-op evidence is recorded | Verify: `cargo check --workspace --all-targets && cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `SP1458-T005` Owner: `specrail` | Dependencies: all implementation tasks | Done when: SpecRail workflow checks pass and the PR body records whether GH-1459 landed in the same implementation PR or an adjacent PR | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1458 && python3 checks/check_workflow.py --repo .`

## Parallelization

`SP1458-T001` should land before hook and docs edits because the manifest policy
is the replacement for the removed environment variable. `SP1458-T002` and
`SP1458-T003` can proceed in parallel after the manifest change if file
ownership is disjoint. `SP1458-T004` and `SP1458-T005` are final gates.

## Verification

- `rg -n "RUSTFLAGS.*Dwarnings|Dwarnings.*RUSTFLAGS" .github .githooks AGENTS.md CLAUDE.md README.md scripts docs`
- manifest audit over `crates/*/Cargo.toml`
- negative intentional-warning check, reverted before commit
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1458`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

GH-1458 is intentionally coordinated with GH-1459. A combined implementation PR
may close both issues only if it satisfies both spec packets. A spec-only PR
must use `Refs #1458` and must not close the issue.
