# Golden Principles

Five non-negotiable principles for AI-assisted code generation.
Derived from OpenAI Harness Engineering best practices.

## GP-01: Produce Executable Artifacts (critical)

Every task must produce a concrete, executable artifact — code that compiles,
tests that pass, scripts that run. Never produce "plan-only" or "suggestion-only"
outputs.

**Validation**: `cargo check` / `npx tsc --noEmit` / `go build ./...` must pass.

## GP-02: Diagnose Before Acting (critical)

When something fails, diagnose the root cause before attempting a fix. Never
guess or apply speculative patches. Read the error, trace the code path,
understand the failure mode.

**Anti-pattern**: Changing random things until CI passes.

## GP-03: Mechanical Execution (critical)

Follow specifications mechanically. Do not improvise, add unrequested features,
or "improve" code beyond the explicit scope. Bug fix = only fix that bug.
Feature = only that feature.

**Anti-pattern**: "While I'm here, let me also refactor..."

## GP-04: Observable Operations (critical)

Every significant operation must emit structured events. Decisions (pass/warn/block),
file changes, command executions, and errors must be logged with timestamps,
session IDs, and context.

**Implementation**: Event log (JSONL) + metrics + quality scoring.

## GP-05: Maintain the Map (critical)

Keep an accurate mental model of the codebase. Before modifying code, understand
the dependency graph, data flow, and existing patterns. Search before creating.
Read before writing.

**Anti-pattern**: Creating a new utility when one already exists.
