# Threads Integration

SpecRail can work with a threads-style orchestration skill, but it must not
depend on one. SpecRail owns the repository workflow contract. A threads skill
owns execution orchestration when work needs parallel lanes, queue gates,
review gates, or closure audits.

## Principle

- SpecRail is the policy and artifact layer.
- Threads is the optional execution layer.
- SpecRail checks run before thread dispatch and again before completion.
- Missing thread support falls back to the normal single-agent SpecRail flow.
- Stable machine-facing IDs stay unchanged across integrations and locales.

## When To Use Threads

Use an available threads skill after SpecRail preflight when the task involves:

- a GitHub issue or pull request queue
- multiple independent implementation lanes
- read-only planner or reviewer lanes
- review thread, CI, or merge-readiness closure
- long-running work that needs a durable handoff

Do not use threads for a small single-file change, ordinary spec drafting, or
any task where all writable files overlap.

## Execution Order

1. Run SpecRail preflight.
   - Load `AGENTS.md`, `workflow.yaml`, `states.yaml`, `labels.yaml`, relevant
     templates, and `skills/specrail-workflow/SKILL.md`.
   - Select the locale.
   - Identify the route, required artifacts, human gates, and verification
     commands.
2. If the task needs queue or parallel orchestration, load the threads skill.
3. Run the threads capability and queue gates.
   - Confirm whether native subagents are available.
   - Fetch remote truth for GitHub queues.
   - Map issues to existing PRs before opening new work.
   - Build a lane map with disjoint writable files.
4. Execute lanes.
   - Planners and reviewers are read-only.
   - Workers own explicit writable paths.
   - The coordinator owns shared verification and final synthesis.
5. Run SpecRail verification.
   - Validate the pack.
   - Validate the spec packet when a spec changed.
   - Preserve human-facing locale rules.
6. Run threads closure audit when GitHub queue or PR state changed.
   - Re-check PR heads, CI, review threads, merge state, and issue closure.
   - Separate remote truth from local worktree state.

## Handoff Contract

Agents should record this block when both systems are active:

```yaml
specrail_threads_handoff:
  specrail:
    route:
    current_state:
    selected_locale:
    required_artifacts:
    human_gates:
    verification_commands:
  threads:
    mode:
    truth_level:
    queue_ledger:
    issue_to_pr_map:
    lanes:
    merge_policy:
    stop_conditions:
```

The block is a handoff artifact, not a schema-stable API. A future evaluator can
turn it into a validated artifact after repeated real use.

## Field Mapping

| SpecRail field | Threads field | Notes |
| --- | --- | --- |
| `route` | `intent_contract.goal` | The route defines the kind of workflow action. |
| `required_artifacts` | `queue_ledger.acceptance_evidence` | Threads records evidence for each queue item. |
| `human_gates` | `merge_policy`, `stop_conditions` | Threads must not bypass SpecRail gates. |
| `verification_commands` | `verification_owner` | One owner runs shared checks for a tranche. |
| `selected_locale` | final report language | Human-facing reports follow SpecRail locale rules. |

## Fallback

If no threads skill or native subagent capability is available, the agent should
continue with the normal SpecRail flow and say that no native threads were
launched. If the user explicitly requested threads, the agent may provide a
prompt pack and lane map instead of pretending parallel execution happened.

## Non-Goals

- Do not vendor a local threads skill into SpecRail.
- Do not make threads required for adoption.
- Do not let threads override SpecRail policy, locale, or human gates.
- Do not add automatic merge or final approval.
- Do not require GitHub for repositories that use SpecRail without GitHub.

## Minimal Agent Rule

For agents such as Codex:

```text
Run SpecRail first. If the task is a queue, parallel-lane, review-thread,
merge-gate, or closure-audit problem and a threads skill is available, use
threads after SpecRail preflight. SpecRail owns policy; threads owns
orchestration. Return to SpecRail verification before reporting completion.
```
