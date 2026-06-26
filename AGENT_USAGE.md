# Agent Usage

SpecRail is primarily for code agents, not for human project management. Humans
own policy and final gates; agents use this repository to decide how to triage,
write specs, prepare PRs, review, and report handoffs without inventing process.

## What The Agent Should Load

When a repository adopts SpecRail, the agent should read these files before
creating issues, specs, PRs, or reviews:

1. `AGENTS.md`
2. `workflow.yaml`
3. `states.yaml`
4. `labels.yaml`
5. the relevant template under `templates/`
6. `skills/specrail-workflow/SKILL.md` when available

The skill is an execution guide. The YAML files and templates are the workflow
contract. The agent should not treat the skill as final authority when it
conflicts with repository policy or human instructions.

Optional integration documents under `integrations/` are loaded only when the
task needs that execution model. They do not replace the core SpecRail contract.

## Basic Agent Flow

1. Search existing issues and PRs before creating new work.
2. Identify the route:
   - `triage_issue`
   - `write_spec`
   - `implement`
   - `review_pr`
   - `fix_ci`
   - `draft_release_note`
3. Default to `write_spec` before `implement` for product-facing,
   architecture, cross-module, public API, workflow-policy, or ambiguous
   behavior changes.
4. Choose direct `implement` only when an approved spec already exists, the
   change is small and mechanical, or the user explicitly asks to skip spec
   creation.
5. Confirm the current state from durable repo state when possible.
6. Create or update the required artifact:
   - issue
   - `specs/GH<issue-number>/product.md`
   - `specs/GH<issue-number>/tech.md`
   - `specs/GH<issue-number>/tasks.md`
   - PR body
   - review result
   - handoff
7. Run the local evaluator before taking the route action:

```sh
python3 checks/route_gate.py --repo . --route write_spec --issue <issue-number> --state ready_to_spec --json
python3 checks/route_gate.py --repo . --route implement --issue <issue-number> --state ready_to_implement --json
```

8. Run deterministic checks before claiming completion:

```sh
python3 checks/check_workflow.py --repo .
python3 checks/check_workflow.py --repo . --spec-dir specs/GH<issue-number>
```

9. Before reporting a PR as merge-ready, collect PR evidence and run:

```sh
python3 checks/github_pr_evidence.py --github-repo OWNER/REPO --pr <pr-number> --json > pr-evidence.json
python3 checks/pr_gate.py --repo . --evidence <evidence.json> --json
```

The GitHub adapter is read-only and only reshapes `gh` output. The PR gate is
offline. GitHub or `threads` may collect evidence such as PR head SHA, CI
status, review threads, merge state, and linked issue references. The gate only
evaluates that evidence and never merges or writes remote state.

If `write_spec` is selected and no GitHub issue number is available, the agent
should search for an existing issue first. If none exists and GitHub workflow is
in scope, create or request a linked issue before writing the numbered spec
packet. A missing issue number is not permission to skip spec creation.

## Optional Threads Integration

If the task is a GitHub issue or PR queue, needs disjoint parallel lanes, or
requires review-thread, CI, merge-gate, or closure-audit handling, load
`integrations/threads.md` after SpecRail preflight. SpecRail still owns policy,
required artifacts, human gates, and deterministic verification. Threads owns
lane orchestration, remote queue truth, and closure audit.

If no threads skill or native subagent capability is available, continue with
the normal single-agent SpecRail flow and report that no native threads were
launched.

## Language Behavior

Harness repository artifacts must be written in English. Use English for:

- issue bodies
- product specs
- tech specs
- PR bodies
- review summaries
- handoffs
- error explanations

Keep stable machine-facing identifiers unchanged:

- action IDs such as `write_spec`
- state IDs such as `ready_to_spec`
- decision values such as `needs_human`
- artifact IDs such as `product_spec`
- paths such as `specs/GH1/product.md`
- commands and CLI flags
- JSON keys and schema field names

## What Exists Today

SpecRail currently provides:

- state and label conventions
- issue/spec/PR templates
- an optional threads integration design
- a Codex-compatible `specrail-workflow` skill
- a deterministic pack validator
- a read-only GitHub PR evidence adapter
- a local evaluator that returns `allowed`, `warn`, `needs_human`, or `blocked`
- an adoption matrix and fixture for real repo pilot evidence:
  `docs/ADOPTION_MATRIX.md` and `examples/adoptions/matrix.json`

This is enough for an agent to follow the process more consistently than raw
README instructions.

## What Does Not Exist Yet

SpecRail does not yet provide:

- GitHub label or issue evidence adapters beyond PR merge-readiness evidence
- automatic issue label checks
- automatic template rendering commands
- automatic merge or final approval
- localized repository artifact generation

Until those exist, agents should treat `checks/route_gate.py` as a local gate and
must report what they verified rather than claiming live GitHub workflow state
from assumptions.

## Human Gates

Agents may draft, propose, review, and diagnose. Agents must not:

- provide final approval
- merge without explicit authorization
- publish private security details
- change repository permissions
- bypass readiness labels or other human gates
