# Learn

## Trigger
Use after fixing a bug or completing a successful implementation to extract reusable rules or skills from the experience.

## Input
Description of the problem encountered and the fix applied. Optionally: diff, error messages, relevant code snippets.

## Procedure
1. **Mode routing**: Determine which output is more valuable:
   - If the fix reveals a recurring anti-pattern → extract a **guard rule**.
   - If the fix reveals a reusable workflow → extract a **skill**.
2. **For rule extraction**:
   a. Identify the pattern that caused the bug (e.g., "unescaped delimiter in prompt construction").
   b. Write a detection heuristic: what code signature triggers this rule?
   c. Write the rule in guard format: ID, severity, description, fix template.
3. **For skill extraction**:
   a. Identify the reusable procedure (e.g., "how to safely wrap external data in LLM prompts").
   b. Write the skill in standard format: trigger, input, procedure, output, constraints.
4. Validate: does the extracted rule/skill correctly describe the original problem? Would it have caught/prevented the issue?
5. Output the rule or skill ready to be added to `.harness/rules/` or `.harness/skills/`.

## Output Format
```
# Learned: <rule or skill name>

Type: RULE | SKILL
Mode: guard rule | reusable workflow

## Problem
<what went wrong or what was discovered>

## Root Cause
<underlying pattern>

## Extracted <Rule|Skill>
<full rule or skill content in standard format>

## Validation
Would this have caught/prevented the original issue? <YES|NO|PARTIAL>
Reasoning: <explanation>
```

## Constraints
- Extract only patterns with clear reuse value — do not generate rules for one-off issues.
- Rules must be falsifiable: a rule that triggers on everything is useless.
- Skills must be actionable: vague guidance is not a skill.
