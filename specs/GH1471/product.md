# Product Spec

## Linked Issue

GH-1471

## User Problem

Static prompt instruction blocks are re-sent uncached on every agent
invocation, and the skills listing injected into every prompt grows linearly
and uncapped with registered skills. The prompt layering built for caching
(`PromptParts`) is flattened at every call site, so the token cost of the
static layer is paid on every turn.

## Goals

- Send static prompt layers through a cacheable channel where the backend
  supports it (Claude Code system prompt, Anthropic API cache_control).
- Cap or filter the skills listing so prompt size stops growing unboundedly.

## Non-Goals

- Changing prompt template content or semantics.
- Building a custom caching proxy.

## Behavior Invariants

1. Backends without caching support receive the same flattened prompt as
   today (no behavior regression).
2. The static layer's content is byte-identical whether sent flattened or via
   the cacheable channel.
3. The skills listing never exceeds the configured cap; selection is
   deterministic for identical skill stores.
