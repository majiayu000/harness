# Exec Plan

## Trigger
Use after a SPEC is approved to generate a milestone-based execution plan for long-running tasks (multi-session).

## Input
Approved SPEC document with goal, scope, affected files, and acceptance criteria.

## Procedure
1. Parse the SPEC to extract all acceptance criteria and affected files.
2. Group affected files into logical milestones (e.g., data model → business logic → API → tests).
3. For each milestone, define: objective, files changed, verification command, expected output.
4. Identify decision points where implementation choices may diverge.
5. Embed recovery instructions: if a session is interrupted, how to resume from each milestone.
6. Record surprises encountered during execution as they occur.
7. Update the plan after each milestone with actual vs expected outcomes.

## Output Format
```
# Exec Plan: <feature name>

## Status
Current milestone: <N>/<total>
Last updated: <timestamp>

## Milestones
### M1: <name>
- Files: <list>
- Command: <verify command>
- Status: pending | in_progress | done | failed

### M2: <name>
...

## Decisions
- <decision point>: <choice made> — <rationale>

## Surprises
- <unexpected finding>: <how resolved>

## Resume Instructions
From M<N>: <how to continue>
```

## Constraints
- Each milestone must have a verification command.
- Do not mark a milestone done until verification passes.
- Surprises section is mandatory — do not leave it empty after execution.
