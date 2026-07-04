# Tech Spec

## Linked Issue

GH-1471

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Prompt layering | `crates/harness-core/src/prompts/mod.rs:44-80` | PromptParts {static, context, dynamic}; `to_prompt_string()` flattens | layering exists, unused |
| Call sites | `triage_pipeline.rs:640,856,994`, `implement_pipeline.rs:200` | flatten per call | switch to structured handoff |
| Claude adapter | `crates/harness-agents/src/claude_adapter.rs` | single prompt arg; no `--append-system-prompt` | cacheable channel available |
| Anthropic API agent | `crates/harness-agents/src/anthropic_api.rs` | plain messages | supports cache_control blocks |
| Skills injection | `crates/harness-core/src/prompts/context.rs:45-61` | uncapped listing per prompt | cap target |

## Proposed Design

1. Extend the agent invocation contract to carry `system_prompt: Option<String>`
   alongside the user prompt; `PromptParts` gains `to_layered()` returning
   (static → system, context+dynamic → user).
2. Claude adapter passes the static layer via `--append-system-prompt`
   (verify against CLAUDE.md arg-order rule; apply to BOTH `claude.rs` and
   `claude_adapter.rs`).
3. Anthropic API agent marks the static block with `cache_control: ephemeral`.
4. Codex adapter keeps the flattened fallback until an equivalent mechanism
   is confirmed.
5. Skills listing: config `prompts.max_skills_in_context` (proposal: 20),
   deterministic selection (freshness/name order) in `context.rs`.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 fallback identical | to_prompt_string retained | golden test: flattened output unchanged |
| P2 layer content identity | to_layered | unit test: concat(layers) == flattened |
| P3 skills cap deterministic | context.rs | unit test with 30 skills → capped, stable order |

## Alternatives Considered

- Only capping skills, no caching — smaller win; the static layer dominates.
- Custom prompt-cache proxy — rejected: complexity, vendor caching exists.
