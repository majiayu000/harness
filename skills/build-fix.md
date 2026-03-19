# Use when: a build or compilation fails — locate root cause and apply the minimal fix to get it green. Triggers on: build error, build failed, compilation error, compile error, cargo error, fix build

<!-- trigger-patterns: build error, build failed, compilation error, compile error, cargo error, fix build -->

## Trigger
Use when a build or compilation fails to locate root cause and apply the minimal fix.

## Input
Build error output (stderr from cargo/tsc/go build/etc.) and the project root path.

## Procedure
1. Parse error output to extract: file, line, column, error code, error message.
2. Group errors by root cause (e.g., one missing type can cascade into 20 errors).
3. Read the file(s) at the reported lines to understand context.
4. Identify the minimal change that resolves the root cause without introducing new issues.
5. Apply the fix.
6. Re-run the build command to verify the fix resolves all errors.
7. If new errors appear, repeat from step 1 (max 3 iterations).

## Output Format
```
# Build Fix

## Error Summary
Command: <build command>
Root cause: <description>
Cascade count: <N> errors from <M> root causes

## Root Causes
1. <file>:<line> — <error> → Fix: <description>

## Applied Changes
- <file>: <what changed>

## Verification
<build command output showing success>
```

## Constraints
- Fix only the root cause — do not refactor surrounding code.
- If the fix requires changing a public API signature, flag it as a breaking change.
- Stop after 3 iterations without progress and report the blocker.
- Never delete tests to make a build pass.
