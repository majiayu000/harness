# Task Plan

## Linked Issue

GH-1441

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1441-T001` Owner: `startup-validation` | Done when: GitHub webhook-capable intake modes validate a normalized non-empty webhook secret during startup and record a sanitized `github_webhook_intake` degradation when unavailable | Verify: `cargo test -p harness-server health github_webhook_intake`
- [ ] `SP1441-T002` Owner: `webhook-secret` | Done when: webhook request handling rejects missing, empty, and whitespace-only configured secrets before signature verification while preserving secret redaction | Verify: `cargo test -p harness-server github_webhook`
- [ ] `SP1441-T003` Owner: `intake-status` | Done when: `/api/intake` reports GitHub mode, active webhook/polling driver state, degradation state, and effective repo entries without removing existing fields | Verify: `cargo test -p harness-server intake_status`
- [ ] `SP1441-T004` Owner: `verification` | Done when: focused route tests, package check, fmt, clippy, and SpecRail workflow checks pass for GH1441 | Verify: `cargo test -p harness-server intake_status && cargo test -p harness-server health && cargo test -p harness-server github_webhook && cargo check -p harness-server --all-targets && python3 checks/check_workflow.py --repo . --spec-dir specs/GH1441`

## Parallelization

T001 and T002 both need the same normalized webhook-secret helper and should be
sequenced to avoid conflicting helper ownership. T003 can follow once the helper
and startup driver status names are stable. T004 is the final verification gate.

## Verification

- `cargo test -p harness-server intake_status`
- `cargo test -p harness-server health`
- `cargo test -p harness-server github_webhook`
- `cargo check -p harness-server --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1441`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

Keep the change scoped to startup validation and status surfaces. Do not change
polling cadence, sprint planning, auto-merge policy, webhook event semantics, or
per-repo intake configuration. The main invariant is that webhook-capable GitHub
intake without a usable secret must never look fully healthy.
