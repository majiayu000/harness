---
paths: "**/*.rs"
---

# GC Learned Rules

Rules extracted by the GC Learn pipeline from adopted remediation drafts.

## LEARN-001: Review loop must distinguish FIXED / WAITING / no-progress states (critical)
severity: critical
A review loop that only checks `is_lgtm()` will treat every non-LGTM response (including FIXED and WAITING) identically, creating infinite retry loops. Any state machine with multiple terminal/intermediate states must branch on all of them. Unhandled states must be an explicit error, not a silent default retry.

## LEARN-002: Guard script exemptions must mirror CLAUDE.md exemptions exactly (high)
severity: high
When CLAUDE.md declares rule exemptions (e.g. RS-03: `Mutex::lock().unwrap()`, `fn main()`), every guard script that enforces that rule must implement the same exclusion patterns. A mismatch between declared exemptions and enforced patterns generates chronic false-positive blocks that erode trust in the guard system.

## LEARN-003: Detect consecutive-fixed rounds as a stuck-loop sentinel (high)
severity: high
3 or more consecutive `result:fixed` entries for the same task/PR without an intervening `complete` or `lgtm` is a reliable signal of a stuck review loop. Guard scripts scanning event logs should count per-task consecutive fixed rounds and exit 1 when the count exceeds a configurable threshold.

## LEARN-004: Failure events must carry a failure_class field (high)
severity: high
Aggregating all failure events under a single label (e.g. `task_failure`) makes it impossible to distinguish guard blocks from build errors from agent errors. Every failure record must include a structured `failure_class` (BuildError / GuardBlock / AgentError / Timeout) so monitoring tools can filter by root cause and measure false-positive rates per class.

## LEARN-005: Declared helper functions must be called at every relevant call site (high)
severity: high
`is_waiting()` existed in `prompts.rs` but was never called in `task_executor.rs` — a declaration-execution gap (U-26). After adding any state-check or classification function, grep all call sites of the function it is meant to complement and verify the new function is invoked in the same branches.

## LEARN-006: Build-failure blocks in agent tasks should be demoted to warnings (medium)
severity: medium
Blocking a task hard on `cargo check` failure prevents the agent's self-correction loop from running. For projects with a complete review cycle, build failures inside a task should emit `decision:warn` rather than `decision:block`, preserving the agent's ability to self-fix before commit. This demotion must be opt-in per project.
