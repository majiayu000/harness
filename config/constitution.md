# Harness Constitution — Immutable Architectural Principles

## GP-01: Auditable Artifacts
All operations must produce auditable artifacts (PR, draft, log). Memory-only state changes are not allowed.

## GP-02: Diagnose Before Fix
Always diagnose before fixing. GC must generate signal report before generating fix drafts.

## GP-03: Deterministic Enforcement
Rule checks must be deterministic. Same input produces same output. Never rely on LLM judgment for compliance.

## GP-04: Observable Operations
Every Turn input/output/tool-call must be recorded to EventStore.

## GP-05: Single Source of Truth
Config and runtime state must be consistent. Single configuration source.
