# Cross Review

## Trigger
Use for adversarial dual-model review on high-stakes changes (security-sensitive code, public API changes, data migrations).

## Input
Diff or PR. Optionally specify primary reviewer model and challenger model.

## Procedure
1. **Round 1 — Primary review**: Run the review skill as primary reviewer. Produce findings categorized P0-P3.
2. **Round 1 — Challenger validation**: Challenger independently reviews the same diff. For each P0/P1 finding from primary: confirm (agree), dispute (disagree with rationale), or escalate (found additional issue).
3. **Convergence check**: If challenger disputes any P0/P1, escalate to Round 2.
4. **Round 2 — Resolution**: Primary re-reviews disputed findings with challenger's rationale. Either maintain (with counter-argument) or retract.
5. **Round 3 — Final arbitration** (if still unresolved): Summarize disagreement for human review. Do not block on unresolved disputes — flag them as OPEN.
6. Produce merged report with consensus findings and open disputes.

## Output Format
```
# Cross Review

Rounds: <N>
Consensus: APPROVE | REQUEST_CHANGES | BLOCK

## Consensus Findings
- [P0] <file>:<line> — <description> (confirmed by both)

## Disputed Findings
- [P1] <file>:<line> — Primary: <claim> | Challenger: <counter>
  Status: OPEN — needs human review

## Retracted Findings
- <finding> — retracted because: <rationale>

## Round Summary
Round 1: Primary <N> findings | Challenger confirmed <N>, disputed <N>
Round 2: Resolved <N> | Still open <N>
```

## Constraints
- Maximum 3 rounds. Unresolved disputes after round 3 are flagged OPEN, not blocking.
- Challenger must provide rationale for every dispute — "disagree" alone is insufficient.
- Consensus P0 findings always block, regardless of dispute status.
