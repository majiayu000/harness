# Use when: you need to run Harness GC signal detection, review drafts, adopt/reject remediations, and feed lessons back into rules/skills. Triggers on: gc run, gc drafts, gc adopt, gc reject, learn rules, learn skills

<!-- trigger-patterns: gc run, gc drafts, gc adopt, gc reject, learn rules, learn skills -->

## Trigger
Use this skill when investigating recurring failures and you want the full GC learning loop:
signal detection -> draft generation -> review/adopt -> rule/skill extraction.

## Input
- Project root path.
- Optional draft id (for adopt/reject).
- Optional project filter (multi-project runtime).

## Procedure
1. **Run signal detection**
   - CLI: `harness gc run <project_root>`
   - RPC: call `gc_run`
   - Output includes detected signals and generated draft count.

2. **Inspect drafts**
   - CLI: `harness gc drafts <project_root>`
   - RPC: call `gc_drafts`
   - Focus on `pending` drafts and verify rationale + artifact targets.

3. **Decide each draft**
   - Adopt: `harness gc adopt <project_root> <draft_id>` or RPC `gc_adopt`
   - Reject: `harness gc reject <project_root> <draft_id> [reason]` or RPC `gc_reject`
   - Adopt may enqueue an execution task (depending on config and artifact presence).

4. **Extract reusable knowledge**
   - Rules: call `learn_rules` for the target project root.
   - Skills: call `learn_skills` for the target project root.
   - These steps convert adopted remediation artifacts into reusable constraints/capabilities.

5. **Check observability**
   - Verify `gc_run` / `gc_adopt` / `gc_reject` / `learn_rules` / `learn_skills` / `self_evolution_tick` events in EventStore.
   - Confirm no silent failures before closing the cycle.

## Output Format
```markdown
# GC Cycle Report

Project: <path>
Signals detected: <N>
Drafts generated: <N>

## Draft Decisions
- Adopted: <id list>
- Rejected: <id list with reasons>

## Learning Results
- Rules learned: <N>
- Skills learned: <N>

## Risks / Follow-ups
- <item>
```

## Constraints
- Do not auto-adopt drafts without explicit instruction.
- Rejections should include concrete reasons for future analysis.
- If learn step fails, report failure explicitly; do not silently continue.
