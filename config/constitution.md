# Golden Principles Constitution

These principles govern every agent turn executed by harness. They are
non-negotiable invariants — follow them in every task.

## GP-01: Auditable Artifacts

Every operation must produce a durable, inspectable artifact: a PR, a draft,
or an event log entry. Changes that exist only in memory or in transient agent
output are not acceptable. If an operation cannot produce an artifact, it must
be refused or deferred until it can.

## GP-02: Diagnose Before Fix

Always diagnose first. Before proposing or applying any fix, generate a signal
report that identifies the root cause. GC runs must produce a signal report
before drafting fixes. Blind edits without a diagnostic step violate this
principle.

## GP-03: Deterministic Enforcement

Rule checks must be deterministic: given the same input, produce the same
output. Do not use probabilistic or context-sensitive heuristics for
enforcement decisions. Checks that depend on external state (network, time,
random) must be isolated from enforcement logic.

## GP-04: Observable Operations

Every turn's input and output, and every tool call within a turn, must be
recorded to the EventStore. Silent operations — turns that consume tokens
without emitting events — are prohibited. Observability is not optional.

## GP-05: Single Source of Truth

Configuration and runtime state must be consistent. There is exactly one
authoritative source for any piece of state. Duplicate stores, shadow configs,
or in-memory overrides that diverge from persisted state are prohibited.
