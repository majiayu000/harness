# Task Plan

## Linked Issue

GH-1473

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1473-T001` Owner: `config` | Dependencies: none | Done when: GitHub intake config exposes a documented discovery driver with stable snake_case values and compatibility-preserving defaults | Verify: `cargo test -p harness-core config::intake`
- [ ] `SP1473-T002` Owner: `intake-builder` | Dependencies: `SP1473-T001` | Done when: `build_intake` registers direct REST GitHub pollers only for poll-capable modes using the REST discovery driver, keeps webhook-only poller-free, and fails closed before registering pollers when workflow runtime storage is unavailable | Verify: `cargo test -p harness-server http::builders::intake`
- [ ] `SP1473-T003` Owner: `github-poller` | Dependencies: none | Done when: `GitHubIssuesPoller` records shared-token throttle state from GitHub rate-limit responses and skips immediate retry for all repos sharing that token until the safe retry time | Verify: `cargo test -p harness-server intake::github_issues`
- [ ] `SP1473-T004` Owner: `intake-status-docs` | Dependencies: `SP1473-T001`, `SP1473-T002`, `SP1473-T003` | Done when: intake status/logs expose the selected discovery driver and throttled/degraded polling state, and docs/examples describe REST-only zero-agent discovery | Verify: `cargo test -p harness-server http::tests::intake_auth_list_tests` and `rg -n "direct_rest|discovery" config/default.toml.example docs/usage-guide.md`
- [ ] `SP1473-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and workspace clippy pass | Verify: focused tests above, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1473`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

`SP1473-T001` and `SP1473-T003` can be developed in parallel if file ownership
is disjoint: config changes stay in `crates/harness-core/src/config/intake.rs`
and poller throttling stays in `crates/harness-server/src/intake/github_issues.rs`.
`SP1473-T002` depends on the config shape. `SP1473-T004` should run after the
driver and throttle behavior are stable. Avoid parallel edits to
`crates/harness-server/src/http/builders/intake.rs`.

## Verification

- `cargo test -p harness-core config::intake`
- `cargo test -p harness-server http::builders::intake`
- `cargo test -p harness-server intake::github_issues`
- `cargo test -p harness-server http::tests::intake_auth_list_tests`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1473`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1473` for the spec PR. The implementation PR should use
`Closes #1473` only after config, poller registration, throttling,
status/docs, tests, and verification land.
