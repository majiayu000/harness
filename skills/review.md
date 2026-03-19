# Use when: reviewing a pull request, checking code quality on a diff, or responding to review comments. Triggers on: code review, review pr, review the, review this, review diff

<!-- trigger-patterns: code review, review pr, review the, review this, review diff -->

## Trigger
Use to perform structured code review on a changeset (diff, PR, or set of modified files).

## Input
Diff or list of changed files. Optionally a PR URL or issue reference.

## Procedure
1. Run guard scripts (check skill) to establish a baseline health report.
2. Read each changed file in full to understand context.
3. Review in priority order:
   a. **Security** (P0): Injection vulnerabilities, hardcoded secrets, path traversal, unvalidated input, prompt injection vectors.
   b. **Logic** (P1): Off-by-one errors, race conditions, incorrect error handling, silent failures, wrong assumptions.
   c. **Quality** (P2): Dead code, duplicate logic, violated naming conventions, missing tests for new code paths.
   d. **Performance** (P3): Unnecessary allocations, N+1 queries, blocking calls in async context.
4. For each finding, assign: severity (P0-P3), file, line, description, suggested fix.
5. Summarize: total findings by severity, overall assessment (approve / request changes / block).

## Output Format
```
# Code Review

Baseline: <guard health grade>
Assessment: APPROVE | REQUEST_CHANGES | BLOCK

## Findings

### P0 — Security
- <file>:<line> — <description>
  Fix: <suggestion>

### P1 — Logic
- <file>:<line> — <description>
  Fix: <suggestion>

### P2 — Quality
- <file>:<line> — <description>

### P3 — Performance
- <file>:<line> — <description>

## Summary
P0: <N> | P1: <N> | P2: <N> | P3: <N>
```

## Constraints
- Any P0 finding requires BLOCK assessment.
- Any P1 finding requires REQUEST_CHANGES assessment.
- Do not suggest stylistic changes unless they violate documented rules.
- Security findings must include an attack scenario, not just "could be unsafe".
