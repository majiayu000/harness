# GH-1734 Technical Decision

Status: deferred; no implementation is authorized.

```json
{"issue":1734,"complete":false,"paths":[],"spec_refs":[]}
```

## Current Architecture

There is no Agent Stack snapshot type or production snapshot consumer. Existing
repository inventory and runtime context types continue to serve their current
owners independently. GH-1733 fingerprint producers are absent after the #1859
revert and must not be assumed by downstream code.

## Superseded Design

The former strict snapshot model, canonical byte grammar, fixed digest vectors,
fingerprint adapters, and implementation path manifest are superseded. They are
available in version history for research but are not normative inputs to a
future implementation.

No readiness-label change may make the historical packet executable. A restart
requires a new product and technical review after the gates in `product.md` are
satisfied.

## Restart Design Order

1. Identify the concrete caller and the decision it must make.
2. Enumerate current typed producer evidence reachable by that caller.
3. Classify stable facts, observation metadata, missing coverage, and errors.
4. Decide ephemeral versus persisted ownership and resource limits.
5. Define the smallest internal model and deterministic projection.
6. Add fingerprint evidence only if an approved GH-1733 producer exists and the
   consumer demonstrates the need.
7. Add a wire format or security claims only for a separately approved
   transport or threat model.

The consumer owns the evidence boundary. The design must not create adapters,
canonicalization rules, or digest fields for hypothetical future consumers.

## Minimum Future Properties

A stable ID must exclude declared volatile metadata and change for every fact
the product contract classifies as behavior-affecting. Missing or failed
observations must remain distinguishable from successful empty observations.
Construction and comparison errors must be typed and must not silently produce
a stable ID.

These are restart constraints, not an approved encoding or implementation.
