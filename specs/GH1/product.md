# Product Spec: Configurable Agent Workflow Evaluator

## Linked Issue

GH-1: https://github.com/majiayu000/specrail/issues/1

## Summary

SpecRail should give code agents a deterministic way to ask "may I do this next?" without hard-coding one repository's labels, paths, or review rules into scripts. The evaluator should read repository workflow configuration, inspect current issue/spec/PR evidence, and return an explainable decision that an agent such as Codex can follow before writing specs, implementing code, reviewing PRs, or drafting release notes.

## Problem

SpecRail already defines a portable workflow pack with states, labels, schemas, templates, and deterministic checks. The next step is to make those rules executable for agents. A naive implementation would add separate scripts like `check_issue_state.py`, `check_spec_packet.py`, and `check_pr_gate.py` with fixed label names and fixed paths. That would undermine SpecRail's portability because each repository has different labels, spec paths, review gates, CI names, and manual-testing expectations.

## Goals

- Make workflow checks configuration-driven rather than hard-coded.
- Let agents run one command before each meaningful action and receive an explicit decision.
- Keep repository maintainers in control of workflow policy.
- Preserve SpecRail's dry-run and advisory-first adoption model.
- Keep hard-coded behavior limited to universal safety boundaries.
- Support localized human-facing workflow text without translating stable machine protocol identifiers.

## Non-Goals

- Build a hosted bot, GitHub App, or Oz-style control plane.
- Automatically mutate labels, comments, reviews, branches, or merge state.
- Replace human readiness labels, final approval, security decisions, or merge authority.
- Enforce one fixed GitHub label taxonomy, spec directory layout, CI provider, or PR template.
- Implement language-specific build or test rules.
- Translate canonical state IDs, action IDs, JSON keys, schema names, or command names.

## Behavior

1. SpecRail exposes a single workflow-checking surface that answers whether a requested action is allowed in the current repository state. Supported initial actions are:
   - `triage_issue`
   - `write_spec`
   - `implement`
   - `review_pr`
   - `fix_ci`
   - `draft_release_note`

2. The workflow-checking surface reads policy from repository configuration files rather than from embedded constants. Repository configuration owns:
   - canonical state names
   - label-to-state mappings
   - action-to-state requirements
   - required artifacts
   - spec path templates
   - review gate requirements
   - verification requirements
   - adoption mode

3. The evaluator returns one of four decisions:
   - `allowed`: the agent may proceed with the requested action.
   - `warn`: the agent may proceed, but should surface missing or weak evidence.
   - `needs_human`: the action is blocked until a human performs a gate such as readiness labeling, spec approval, security decision, final review, merge, or release approval.
   - `blocked`: the action violates configured workflow rules or universal safety boundaries.

4. Every decision includes an explanation. The response must identify:
   - the requested action
   - the inferred current state
   - the policy rules that were applied
   - evidence that satisfied the rules
   - missing evidence
   - next allowed actions, when known

5. The evaluator never silently treats missing data as success. If issue labels, PR metadata, CI status, spec files, or configured artifacts are unavailable, the result is `warn`, `needs_human`, or `blocked` according to the configured action and adoption mode.

6. The evaluator distinguishes universal safety rules from repository policy. Universal safety rules are always enforced and include:
   - agents must not merge
   - agents must not provide final approval
   - agents must not force push unless an explicit external instruction permits it
   - agents must not publish secrets or private security details
   - agents must not change repository permissions
   - agents must not bypass configured human gates

7. Repository policy is configurable. A repository may choose labels such as `ready-to-spec`, `ready_for_spec`, or `Ready for Spec`, and SpecRail should treat them equivalently when configured to map to the same canonical state.

8. Spec paths are configurable. A repository may use `specs/GH123/product.md`, `specs/gh-123/PRODUCT.md`, `docs/specs/123/product.md`, or another declared layout. The evaluator validates the configured layout instead of assuming one global path.

9. Adoption mode changes enforcement strength:
   - `dry_run`: report decisions without failing commands or CI.
   - `advisory`: return non-zero only for universal safety violations and malformed configuration.
   - `required`: return non-zero for configured blocking gates.

10. When the requested action is `write_spec`, the evaluator verifies that the linked issue is in a configured state that permits spec drafting and reports which product and tech spec artifacts should be created.

11. When the requested action is `implement`, the evaluator verifies that the issue is ready for implementation or that the configured workflow permits implementation after approved specs. If the issue still needs a spec or human readiness gate, the decision is `needs_human` or `blocked`.

12. When the requested action is `review_pr`, the evaluator verifies that the PR is linked to acceptable durable work, required artifacts are present, verification evidence is available, and the agent review is advisory rather than final.

13. When the requested action is `fix_ci`, the evaluator allows diagnostic and fix work when CI evidence exists or when the repository configuration permits local verification as a substitute. It must not claim CI is green from stale or unavailable evidence.

14. The command-line output supports both human-readable text and JSON. JSON is the stable integration format for agents.

15. A code agent using SpecRail can include the evaluator result in its preflight, handoff, or PR body without rewriting the reasoning by hand.

16. Human-facing evaluator text in this Harness adoption is English-only.
    Issue templates, PR templates, agent summaries, error explanations, and
    handoff text must remain English to match repository policy.

17. Machine-facing identifiers remain stable. Values such as `write_spec`,
    `ready_to_spec`, `needs_human`, `blocked`, schema keys, command names, and
    artifact IDs are unchanged.

18. Language policy follows repository policy first. For this repository, the
    selected artifact language is always English even when chat with a user
    happens in another language.

19. Missing display text must not change the evaluator decision. The evaluator
    reports stable message codes and English fallback text.

20. The evaluator should be useful without network access when the caller provides local metadata files or artifact paths. GitHub integration may be added later as an adapter, but the core decision logic should not depend on live GitHub API calls.

21. Misconfiguration is treated as a first-class result. If workflow configuration cannot be parsed, references unknown states, defines impossible transitions, or maps one action to conflicting gates, the evaluator reports `blocked` with configuration errors.

22. The default SpecRail pack remains generic. Repository-specific labels, path conventions, CI names, PR rules, and language policy belong in consumer overlays or explicit config files, not in the evaluator source code.

## Acceptance Criteria

- [ ] A code agent can run one configured evaluator command before `write_spec`, `implement`, `review_pr`, `fix_ci`, or `draft_release_note` and receive `allowed`, `warn`, `needs_human`, or `blocked`.
- [ ] The evaluator reads label names, canonical state mappings, action gates, artifact paths, and verification requirements from config rather than hard-coded script branches.
- [ ] Missing issue labels, PR metadata, spec files, CI evidence, or artifact evidence never produce silent success.
- [ ] Universal agent safety boundaries remain enforced regardless of repository configuration.
- [ ] Existing workflow-run automation modes remain distinguishable from evaluator severity modes.
- [ ] Human-facing evaluator messages, issue/PR templates, and agent summaries follow the repository English-only policy without changing stable protocol IDs.
- [ ] The default SpecRail pack remains generic, with repository-specific behavior expressed through overlays or explicit config.
