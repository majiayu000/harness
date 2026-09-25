# GH-1733 Technical Decision

Status: deferred; no implementation is authorized.

```json
{"issue":1733,"complete":false,"paths":[],"spec_refs":[]}
```

## Current Architecture

There is no runtime or MCP fingerprint producer in production. The Agent Stack
component model remains independent of fingerprint collection, and no workflow,
server, persistence, or agent-adapter path may assume fingerprint evidence is
available.

The implementation introduced by PR #1859 is reverted because it has no
consumer and exposes process-execution behavior that would require a stronger
contract than the current need supports. The strict implementation from PR
#1912 is intentionally not merged.

## Restart Design Order

After every product restart gate in `product.md` is satisfied, design in this
order:

1. Define the consumer input and output types.
2. Define whether evidence is ephemeral or persisted.
3. Define exact executable and MCP fields that affect the consumer.
4. Define collection limits and typed failure behavior.
5. Define the minimum platform-specific implementation.
6. Add a security boundary only if the approved threat model requires one.

The consumer contract owns the minimum evidence shape. A collector must not
invent fields, platform machinery, or trust claims for hypothetical future use.

## Minimum Safety Properties

Any future executable probe must use a closed, runtime-owned version command;
must not accept arbitrary argument vectors; must not receive setup secrets; and
must bound process lifetime and output while recording collection failure. A
probe failure yields no successful version claim.

Any future MCP digest must preserve the exact behavior-affecting fields selected
by the consumer, reject ambiguous input, apply explicit resource limits, and
keep observation metadata separate from stable digest input.

These are restart constraints, not an approved implementation design.

## Superseded Material

The former runtime observation, runtime product, and runtime supervision
documents are removed from the maintained specification. Their history and the
#1912 branch may be consulted as research, but neither is normative.

GH-1734 currently assumes the superseded strict envelope. It must remain
deferred and be rewritten from its real production consumer before either issue
can return to a ready state.
