# Common Coding Style Rules

## CS-01: No public API signature changes without explicit request (high)
Breaking changes require user confirmation and MAJOR version bump.

## CS-02: No premature abstractions (high)
Wait for the third repetition before extracting. 3 lines of duplication > 1 premature abstraction.

## CS-03: No unrequested features (high)
Bug fix scope is strictly locked. Do not refactor surrounding code.

## CS-04: No guessing user intent (high)
When uncertain, mark as DEFER or ask.

## CS-05: No style changes mixed with fixes (medium)
Style changes belong in separate commits.

## CS-06: Immutability preferred (medium)
Create new objects rather than mutating. Function parameters are read-only.

## CS-07: File size control (medium)
200-400 lines typical, 800 lines maximum. Split above 800.

## CS-08: State machines must branch on all states (critical)
A state machine that only checks one terminal state (e.g. `is_lgtm()`) and defaults all others to retry will create infinite loops. Every intermediate state (FIXED, WAITING, no-progress) must have an explicit branch. Unhandled states must be an explicit error, not a silent default.

## CS-09: Guard exemptions must mirror declaration exemptions (high)
When CLAUDE.md or config declares rule exemptions (e.g. RS-03: `Mutex::lock().unwrap()`), every guard script enforcing that rule must implement the same exclusion patterns. Mismatch generates chronic false-positive blocks that erode trust in the guard system.

## CS-10: Failure events must carry a structured class (high)
Aggregating failures under a single label (e.g. `task_failure`) makes root cause invisible. Every failure record must include a `failure_class` field (BuildError / GuardBlock / AgentError / Timeout) so monitoring can filter by cause and measure false-positive rates.

## CS-11: Build-failure blocks should be demoted to warnings in agent tasks (medium)
Hard-blocking on `cargo check` failure prevents the agent's self-correction loop. For projects with a review cycle, build failures inside agent tasks should emit `decision:warn`, preserving the agent's ability to self-fix. Must be opt-in per project.
