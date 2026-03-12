# Preflight

<!-- trigger-patterns: preflight, before changes, 3-5 files, medium complexity, identify constraints -->

## Trigger
Use before medium-complexity changes (3-5 files) to identify constraints and risks before writing any code.

## Input
Task description and the set of files expected to be modified.

## Procedure
1. Read each affected file completely to understand current implementation.
2. Identify all callers of functions that will change (grep for usages).
3. Check for existing tests covering the affected code paths.
4. Scan applicable rules (security, coding style, data consistency).
5. Estimate complexity: count distinct files, external dependencies, test coverage gaps.
6. List constraints that must not be violated during implementation.
7. Flag any pre-conditions that must be true before starting.

## Output Format
```
# Preflight: <task name>

## Affected Files
- <file>: <current behavior summary>

## Callers at Risk
- <function/method>: called from <location>

## Test Coverage
- <file>: <covered | uncovered | partial>

## Applicable Rules
- <rule ID>: <what it requires>

## Constraints
- MUST: <constraint>
- MUST NOT: <constraint>

## Pre-conditions
- [ ] <check 1>
- [ ] <check 2>

## Complexity Estimate
Files: <N> | External deps: <N> | Coverage gaps: <N>
```

## Constraints
- Do not begin implementation if any MUST constraint cannot be satisfied.
- Flag uncovered code paths as technical debt requiring new tests.
- Re-run preflight if scope expands beyond original estimate.
