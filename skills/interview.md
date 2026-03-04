# Interview

## Trigger
Use before starting large features (6+ files) to deeply understand requirements, constraints, and risks.

## Input
Task description or feature request from the user.

## Procedure
1. Parse the task description to identify scope keywords and ambiguous terms.
2. Ask targeted questions covering: goals, non-goals, affected systems, performance requirements, security implications, rollback strategy.
3. Probe edge cases: error states, concurrent access, data migration, API backward compatibility.
4. Clarify acceptance criteria: what does "done" look like? How is it tested?
5. Identify dependencies: external APIs, third-party services, team dependencies.
6. Surface technical trade-offs: build vs buy, simplicity vs extensibility.
7. Synthesize answers into a structured SPEC document.

## Output Format
```
# SPEC: <feature name>

## Goal
<one-sentence goal>

## Scope
- In scope: <list>
- Out of scope: <list>

## Constraints
- <constraint 1>
- <constraint 2>

## Affected Files
- <file or module>: <reason>

## Acceptance Criteria
- [ ] <criterion 1>
- [ ] <criterion 2>

## Risks
- <risk>: <mitigation>

## Open Questions
- <question>: <status>
```

## Constraints
- Do not start implementation until SPEC is approved.
- Flag any requirement that conflicts with existing architecture.
- If any acceptance criterion is untestable, flag it immediately.
