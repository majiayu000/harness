# Real Cursor evaluation run — unexecuted template

Copy this document into the run's evidence directory. Empty fields are missing
evidence, never a pass. This template itself is not a baseline report.

## Run identity

- Run ID / date:
- Catalog revision and admitted case IDs:
- Variant and hypothesis:
- Harness commit and saved working-tree diff:
- Prompt snapshot and workflow configuration:
- Cursor CLI version / requested model / observed model if available:
- Environment, resource budget, concurrency and memory isolation:
- Candidate input snapshot and withheld evaluator evidence location:

## Per-attempt record

Repeat for each case and actual attempt.

- Case ID / attempt / variant:
- Frozen start revision / final revision:
- Workflow and job IDs / actual Cursor session IDs by stage:
- Transcript, tool-output and patch paths:
- Review verdict and revision reviewed / actual repair rounds:
- Evaluator verdict and specific acceptance evidence:
- Independent evaluator identity and any human adjudication:
- Missed or false review findings / handoff omissions:
- Harness terminal state and stop reason:
- Environment failures or untested requirements:
- Elapsed time / recorded cost and tokens, or `unknown`:
- Human intervention, or explicit `none`:
- Leakage or reference-solution exposure:

## Paired comparison

| Case | Baseline attempt result | Candidate attempt result | Evidence-backed difference |
|---|---|---|---|

- Scheduled / executed / evaluable / independently accepted task counts:
- Incomplete, environment-failed, assisted and inconclusive counts:
- False completions and avoidable stops, with denominators:
- Improved / regressed / unchanged / inconclusive cases:
- Repeated-run outcomes (actual executions, not inferred trials):
- Time and available cost for comparable-quality outcomes:
- Coverage gaps, uncertainty, and held-out result if genuinely available:
- Decision and supporting evidence; unresolved questions:
