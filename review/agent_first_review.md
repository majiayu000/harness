# Agent First Review

Agent review is advisory. It must produce findings and evidence, not final
approval.

## Check

- Scope matches linked issue and spec.
- Implementation satisfies acceptance criteria.
- Tests cover changed behavior.
- User-visible behavior is not silently degraded.
- Security-sensitive areas are identified.
- PR template is complete.
- Release note need is explicit.

## Output

Return structured findings:

```json
{
  "verdict": "findings|no_findings",
  "blocking_findings": [],
  "non_blocking_findings": [],
  "missing_evidence": [],
  "recommended_human_reviewers": []
}
```

## Boundary

Do not approve, merge, close issues, or mark security findings public.
