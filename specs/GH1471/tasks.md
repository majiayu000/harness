# Task Plan

## Linked Issue

GH-1471

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1471-T001` Owner: `core-prompts` | Dependencies: none | Done when: `AgentRequest` can carry optional prompt-layer data while preserving the existing flattened `prompt` field | Verify: `cargo test -p harness-core prompts`
- [ ] `SP1471-T002` Owner: `executor` | Dependencies: `SP1471-T001` | Done when: issue triage, planning, and implementation paths preserve `PromptParts` into `AgentRequest` where those builders already return layered prompts | Verify: focused `cargo test -p harness-server <prompt-layer-test-filter>`
- [ ] `SP1471-T003` Owner: `claude-adapters` | Dependencies: `SP1471-T001` | Done when: both Claude Code spawn paths pass static instructions through `--append-system-prompt`, keep `-p <dynamic prompt>` ordering, and enable `--exclude-dynamic-system-prompt-sections` for layered prompts | Verify: `cargo test -p harness-agents claude claude_adapter`
- [ ] `SP1471-T004` Owner: `skills` | Dependencies: none | Done when: broad available-skills listing is capped or deterministically filtered, matched skill full-content injection remains uncapped, and usage events still record matched skills | Verify: focused `cargo test -p harness-server inject_skills`
- [ ] `SP1471-T005` Owner: `verification` | Dependencies: all implementation tasks | Done when: focused tests, server check, SpecRail checks, formatting, and clippy pass | Verify: focused tests above, `cargo check -p harness-server --all-targets`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1471`, `python3 checks/check_workflow.py --repo .`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

Keep implementation mostly serial. `AgentRequest` shape changes affect adapters,
executor call sites, tests, and prompt persistence. Skills-list capping can be
implemented independently after the core request shape is settled.

## Verification

- `cargo test -p harness-core prompts`
- `cargo test -p harness-agents claude claude_adapter`
- focused `cargo test -p harness-server <prompt-layer-test-filter>`
- focused `cargo test -p harness-server inject_skills`
- `cargo check -p harness-server --all-targets`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1471`
- `python3 checks/check_workflow.py --repo .`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

Use `Refs #1471` for the spec PR. The implementation PR should use
`Closes #1471` after the layered Claude Code path, skills cap, fallback tests,
and verification land.
