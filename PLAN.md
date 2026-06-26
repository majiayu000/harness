# Plan

SpecRail is an agent-first workflow contract. It is not trying to be another
human-facing project-management guide. The central question is:

```text
What should a code agent do next, and what evidence proves it may do that?
```

## Current Position

SpecRail v0.1 is a portable workflow pack:

- state machine
- labels
- templates
- schemas
- review guides
- deterministic pack validation
- localized presentation assets
- a Codex-compatible workflow skill

This is useful, but it is not yet stronger than mature systems such as Warp/Oz.
Warp has a real queue, readiness labels, spec PRs, CI, automated review, and SME
handoff. SpecRail is still a reusable contract that needs repeated real use.

## Design Principles

1. Repository configuration owns policy. Code interprets policy.
2. Agents can draft and diagnose, but humans own final gates.
3. Missing evidence is never silent success.
4. Stable machine IDs stay in English across locales.
5. Human-facing text follows the selected locale.
6. Deterministic checks come before LLM automation.
7. Automation starts in dry-run or advisory mode.
8. Spec-first is the default for ambiguous, architecture, product-facing,
   public API, cross-module, and workflow-policy changes.

## Why The Skill Exists

Templates define output shape. They do not tell an agent when to search, which
state transition is allowed, how to choose locale, or which values must not be
translated.

`skills/specrail-workflow/SKILL.md` exists to make those operating rules explicit
for Codex-style agents. It is not perfect or final. It is a v0.1 execution guide
that should be tested against real tasks and then tightened.

## Roadmap

### Phase 1: Manual Contract

- Keep templates and schemas small.
- Use `AGENT_USAGE.md` and `skills/specrail-workflow/SKILL.md` for agent runs.
- Validate the pack with `checks/check_workflow.py`.
- Record failures as changes to templates, docs, checks, or skill instructions.

### Phase 2: Configurable Evaluator

Implement and harden an offline evaluator that reads repo config and evidence,
then returns:

- `allowed`
- `warn`
- `needs_human`
- `blocked`

The first local route gate lives at `checks/route_gate.py`. It is intentionally
read-only and local-evidence based. Next hardening steps are richer artifact
validation, localized display messages, and GitHub evidence adapters. JSON keys
and stable IDs must stay language-independent.

### Phase 3: Evidence Adapters

Add adapters that collect evidence from GitHub:

- issue labels
- linked PRs
- CI status
- review state
- review threads

Adapters should produce evidence JSON. They should not own policy.

The first merge-readiness evaluator is `checks/pr_gate.py`. It consumes local
evidence JSON and checks PR head, CI, review threads, merge state, linked issue,
and human merge authorization. A later GitHub adapter can collect that evidence,
but the policy decision should stay in the evaluator.

The first GitHub evidence adapter is `checks/github_pr_evidence.py`. It uses
`gh pr view` and `gh api graphql` to collect PR merge-readiness evidence and
prints JSON that `checks/pr_gate.py` can evaluate. It does not write labels,
comments, reviews, thread state, branches, or merges.

### Phase 4: Agent Installation

Make SpecRail easy to give to agents:

- copy pack into a repo
- install or reference `skills/specrail-workflow`
- optionally set `presentation.default_locale: zh-CN`
- run deterministic checks before and after agent work
- use optional integration docs, such as `integrations/threads.md`, when the
  task needs queue orchestration, parallel lanes, or closure audit

### Phase 5: Automation

Only after manual validation on real tasks:

- comment-only checks
- label suggestions
- PR gate comments
- stale workflow reports

Do not add automatic merge, final approval, or public security-disclosure
automation.

## Success Criteria

SpecRail becomes useful when an agent can join a new repo, read the pack, and
produce issue/spec/PR artifacts that maintainers can review without first
explaining the workflow in chat.

SpecRail becomes trustworthy when repeated real runs produce fewer process
mistakes, not just nicer templates.

## Optional Integrations

Integrations are advisory execution designs, not required runtime dependencies.
They let SpecRail describe how an agent should combine the core contract with a
separate orchestration skill.

The first integration is `integrations/threads.md`. It keeps the boundary
explicit:

- SpecRail owns policy, artifacts, locale, human gates, and deterministic checks.
- Threads owns lane maps, queue gates, review-thread truth, merge gates, and
  closure audit.
- The handoff remains a lightweight text/YAML artifact until repeated real runs
  justify turning it into a schema and validator check.
