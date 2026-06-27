---
name: specrail-workflow
description: Use when working in this repository with SpecRail for issue-first, spec-first, AI-assisted development. Handles triage, product specs, tech specs, PR bodies, review summaries, and handoffs using English-only repository artifacts.
---

# SpecRail Workflow

Use this skill as the entrypoint for SpecRail-governed repository work.

## Startup

1. Search before creating a new issue, spec, template, policy, schema, or workflow.
2. Read applicable `AGENTS.md`.
3. Read `workflow.yaml`, `states.yaml`, `labels.yaml`, and relevant templates.
4. Identify the route:
   - `triage_issue`
   - `write_spec`
   - `implement`
   - `review_pr`
   - `fix_ci`
   - `draft_release_note`
5. Run `checks/route_gate.py` for the selected route when the repository includes
   it. Treat `allowed` as permission to proceed, `warn` as proceed-with-caution,
   `needs_human` as a maintainer gate, and `blocked` as a stop condition.

Default to `write_spec` before `implement` for product-facing, architecture,
cross-module, public API, workflow-policy, or ambiguous behavior changes.
Choose direct `implement` only when the change is already covered by an
approved spec, is a small mechanical fix, is a test-only/doc-only correction, is
a focused CI fix, or the user explicitly asks to skip spec creation.

If `write_spec` is selected and no GitHub issue number is available, search for
an existing issue first. If none exists and GitHub workflow is in scope, create
or request a linked issue before writing `specs/GH<issue-number>/product.md` and
`tech.md`. Do not treat a missing issue number as permission to skip the spec.

## Language

Harness repository artifacts must be written in English. Use English for issue
bodies, `product.md`, `tech.md`, PR bodies, review summaries, handoffs, error
explanations, and generated templates even when chat with the user happens in
another language.

Keep stable machine-facing identifiers unchanged:

- action IDs such as `write_spec`
- state IDs such as `ready_to_spec`
- decision values such as `needs_human`
- artifact IDs such as `product_spec`
- file paths such as `specs/GH1/product.md`
- command names and CLI flags
- JSON keys and schema field names

## Spec Creation

For feature work that needs a spec:

1. Confirm or create a linked GitHub issue before creating a numbered spec.
2. Use `specs/GH<issue-number>/product.md` and `specs/GH<issue-number>/tech.md`.
3. Use the root templates in `templates/`.
4. Keep behavior in product spec and implementation plan in tech spec.
5. Run:

```sh
python3 checks/route_gate.py --repo . --route write_spec --issue <issue-number> --state ready_to_spec --json
python3 checks/check_workflow.py --repo . --spec-dir specs/GH<issue-number>
```

Before implementation, run:

```sh
python3 checks/route_gate.py --repo . --route implement --issue <issue-number> --state ready_to_implement --json
```

## Merge Readiness

Before reporting a pull request as merge-ready, collect PR evidence and run the
offline gate when available:

```sh
python3 checks/github_pr_evidence.py --github-repo <owner/repo> --pr <pr-number> --json > <evidence.json>
python3 checks/pr_gate.py --repo . --evidence <evidence.json> --json
```

`checks/github_pr_evidence.py` is a read-only collector for GitHub CLI output,
not a policy engine and not remote automation. The evidence may come from that
adapter, a threads lane, or another read-only adapter. It should include PR head
SHA, linked issue, CI/check rollup, review decision, review-thread resolution,
merge state, and human merge authorization. `allowed` means the evidence is
merge-ready. `needs_human` means deterministic checks passed but merge
authorization is missing. `blocked` means do not merge.

## Optional Threads Integration

If the task is a GitHub issue or PR queue, needs disjoint parallel lanes, or
requires review-thread, CI, merge-gate, or closure-audit handling, read
`integrations/threads.md` after this startup flow and use an available threads
skill for orchestration.

Keep the boundary clear:

- SpecRail owns policy, required artifacts, human gates, and
  deterministic verification.
- Threads owns lane maps, queue gates, remote truth refresh, review-thread
  handling, and closure audit.
- If no threads skill or native subagent capability is available, continue with
  the single-agent SpecRail flow and report that no native threads were
  launched.

## Agent Boundaries

Agents may draft, review, diagnose, and propose labels.

Agents must not:

- provide final approval
- merge without explicit user authorization
- force push without explicit user authorization
- publish secrets or private security details
- change repository permissions
- bypass human gates

## Output

When reporting completion, include:

- issue or PR link, if created
- spec paths
- English-only repository artifact policy
- stable IDs kept unchanged
- verification commands and results
- PR gate decision when merge readiness was evaluated
