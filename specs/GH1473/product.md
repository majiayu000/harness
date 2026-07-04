# Product Spec

## Linked Issue

GH-1473

## User Problem

Poll-mode backlog scanning is executed by an LLM agent (codex_exec), so idle
polling costs agent invocations proportional to repo count. Deployments
without a public webhook endpoint have no cheap polling option; the direct
REST poller exists in the codebase but is not registered.

## Goals

- An agent-free discovery mode for poll-based intake: full sweep spawns zero
  agent processes.
- Discovered issues enter the same dedupe/dispatch path as webhook events.

## Non-Goals

- Removing the agent-driven backlog analysis path.
- Changing webhook/hybrid mode semantics.

## Behavior Invariants

1. In REST discovery mode, a backlog sweep performs zero agent spawns.
2. REST discovery respects GitHub rate limits via throttling.
3. An issue discovered by REST and by webhook resolves to one task
   (external-id dedupe), never two.
