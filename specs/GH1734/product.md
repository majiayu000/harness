# GH-1734 — Deterministic Agent Stack Snapshots

Status: deferred.

Stable ID: `ASC-005`.

## Decision

Harness has no production call path that builds, stores, compares, or presents
an Agent Stack snapshot. The previous specification designed a strict aggregate
around the GH-1733 fingerprint envelope, but that producer is also deferred and
its unused implementation is being removed.

The former strict snapshot contract is therefore not approved for
implementation. This document preserves the product problem and restart gates
without preselecting a wire format, persistence model, or security boundary.

## Problem Worth Preserving

A future product may need to decide whether two observations describe the same
behavior-affecting Agent Stack. Volatile observation metadata such as collection
time or run identity must not create false drift, while facts that the consumer
actually treats as behavior-affecting must remain visible.

## Restart Gates

Work must not restart until all of the following exist:

1. A production call path that creates and consumes a snapshot.
2. A named user-visible decision based on equality or drift.
3. A storage owner, retention policy, and privacy boundary, or an explicit
   decision that snapshots are ephemeral.
4. A current inventory of the typed evidence already available to that call
   path.
5. An approved decision on whether the stable ID is diagnostic identity or a
   security boundary.

The existence of ASC-001 through ASC-004 backlog items does not satisfy these
gates.

## Future MVP Boundary

If the restart gates are satisfied, start with:

- one internal typed snapshot used by the named consumer;
- an explicit projection containing only consumer-required stable facts;
- deterministic ordering for representation-only collections;
- observation metadata outside the stable identity projection;
- explicit coverage and collection failures;
- sensitivity tests proving which fact changes do and do not change identity.

Fingerprint inputs are optional. Add them only after GH-1733 independently
satisfies its restart gates and the snapshot consumer requires them.

## Non-Goals for the MVP

- A public snapshot wire format without a real transport consumer.
- Automatic persistence, APIs, dashboards, signing, or attestation.
- A universal aggregate of every possible Agent Stack fact.
- Reusing the superseded strict envelope or conformance vectors.
- Treating missing evidence as proof that a component does not exist.

## Done for the Deferred State

- No complete implementation manifest or actionable task plan remains.
- GH-1734 is not marked ready while its consumer is absent.
- Respecification must start from current production types and the real call
  path, not from the historical strict packet.
