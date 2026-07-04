# Task Plan

## Linked Issue

GH-1450

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1450-T001` Owner: core-config | Done when: `[isolation]` schema parses tiers, trust rules, and network allowlist, accepting `microvm` in the parser but rejecting it at startup with an explicit error | Verify: `cargo test -p harness-core isolation_config`
- [ ] `SP1450-T002` Owner: server-intake | Done when: intake classifies author trust from `author_association` and persists the class on the workflow row | Verify: `cargo test -p harness-server intake_trust`
- [ ] `SP1450-T003` Owner: workflow-dispatch | Done when: tier resolution is a pure function (task metadata + config → tier + reason) recorded in run evidence at dispatch | Verify: `cargo test -p harness-workflow tier_resolution`
- [ ] `SP1450-T004` Owner: server-health | Done when: startup probes required tiers, unavailable tiers surface as degraded health, and matching intake is refused rather than silently downgraded (GH-1441 pattern) | Verify: `cargo test -p harness-server isolation_health`
- [ ] `SP1450-T005` Owner: agents-spawn | Done when: a shared spawn contract trait has host and container implementations; container runs mount only the task workspace, apply the egress allowlist, and inherit no operator env secrets | Verify: `cargo test -p harness-agents container_spawn`
- [ ] `SP1450-T006` Owner: agents-credentials | Done when: per-task repo-scoped GitHub App tokens are minted with TTL ≤ task timeout, injected into container env, and revoked at teardown | Verify: `cargo test -p harness-agents scoped_token`
- [ ] `SP1450-T007` Owner: docs+image | Done when: a reference agent image (claude/codex CLIs) exists with digest pinning documented, plus the operator guide for enabling the container tier | Verify: container fixture flow passes against the pinned image

## Parallelization

- T001 first. Then T002 (intake files) ∥ T003 (resolution module) ∥ T007 (image/docs).
- T004 after T001; T005 after T003; T006 ∥ T005 (credential module vs spawn module).

## Verification

- `cargo test --workspace`
- Integration: container issue→PR fixture flow; refusal paths (docker absent, mint failure); cancel-mid-run teardown leaves no container or credential.
- Manual: non-collaborator issue on a public repo runs in the container tier; container env shows the scoped token only.

## Handoff Notes

- Docker/OCI invocation lives in the agent-spawn layer; the crate-level gh/git ban concerns GitHub state mutations, which remain in agent prompts.
- `microvm` is name-reserved only; do not scaffold an engine in this issue.
