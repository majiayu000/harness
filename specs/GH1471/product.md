# Product Spec

## Linked Issue

GH-1471

## User Problem

Harness already models issue prompts as `PromptParts` with static, context, and
dynamic layers. The call sites immediately flatten those layers into a single
string before agent execution, so stable instructions are resent as normal user
prompt text on every turn.

The skills listing also injects every available skill name and description into
agent prompts. As the skill store grows, prompt size grows linearly even when
most skills are irrelevant to the task.

## Goals

- Preserve `PromptParts` layer boundaries through the agent request boundary.
- Use a cache-friendly channel for at least one supported backend, starting
  with Claude Code because the local CLI exposes `--append-system-prompt` and
  `--exclude-dynamic-system-prompt-sections`.
- Keep flattened prompt behavior as the fallback for backends without a
  cacheable prompt channel.
- Cap or relevance-filter the all-skills listing so prompt size is bounded.
- Keep matched skill content injection and `skill_used` recording semantics.

## Non-Goals

- Rewriting prompt templates or changing their user-visible instructions.
- Building a custom prompt-caching proxy.
- Adding network calls to inspect provider cache state.
- Removing the full flattened prompt path for Codex, Anthropic API, tests, or
  any backend that cannot consume prompt layers.
- Changing task artifact persistence or prompt redaction semantics beyond
  preserving the effective prompt for audit.

## User-Visible Behavior

For Claude Code execution, stable prompt instructions should be passed through a
system-prompt append path while dynamic task content remains the main `-p`
prompt. Operators should see cache-related token metrics already parsed from
Claude output when the provider reports them.

For other backends, tasks should behave as they do today by using the flattened
prompt string.

When many skills exist, the available-skills listing should include only a
bounded subset or a relevance-filtered subset. Matched skills should still
appear in the relevant-skills section with full content.

## Acceptance Criteria

- [ ] `PromptParts` can be converted into a layered agent request without
      losing the existing flattened string behavior.
- [ ] Claude Code receives the static layer through `--append-system-prompt`
      or an equivalent cache-friendly system-prompt path.
- [ ] Claude Code uses `--exclude-dynamic-system-prompt-sections` when the
      cache-friendly path is active, unless a compatibility check requires a
      fallback.
- [ ] Codex and Anthropic API keep a deterministic flattened fallback path.
- [ ] The available-skills listing is capped or filtered, with focused tests
      proving prompt size does not grow unboundedly with registered skills.
- [ ] Matched skills still inject full content and still record usage events.
- [ ] Existing prompt audit/redaction tests continue to pass.

## Edge Cases

- Empty static/context layers should not emit empty CLI arguments.
- A backend that does not support layered prompts must receive exactly the
  flattened prompt it would have received before this change.
- Validation retry prompts that are built from a prior prompt must preserve the
  correct flattened content even when the initial request was layered.
- Skill caps must be deterministic so repeated runs with the same skill store
  and prompt produce the same injected prompt.

## Rollout Notes

Use `Refs #1471` for the spec PR. The implementation PR should use
`Closes #1471` after the layered request path, skills cap, docs if needed, and
tests land.
