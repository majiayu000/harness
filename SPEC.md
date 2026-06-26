# SpecRail v0.1 Specification

## Problem

AI-assisted coding makes implementation cheaper, but repository workflow often
stays implicit. Without a shared state machine, agents and humans disagree about
whether a request needs clarification, a product spec, implementation, review,
or release notes.

SpecRail standardizes the repository workflow contract before automation.

## Scope

This pack defines:

- issue readiness states
- feature spec packets
- pull request review gates
- agent-first review boundaries
- human-final review boundaries
- deterministic workflow checks

## Non-Goals

- Building a hosted Oz-style control plane.
- Replacing GitHub issues or pull requests.
- Granting agents final approval or merge authority.
- Handling private security reports through public issues.
- Enforcing language-specific build rules.

## Workflow Model

```text
new_issue
  -> needs_info | duplicate | security_private | triaged

triaged
  -> ready_to_spec | ready_to_implement | reserved_internal

ready_to_spec
  -> spec_pr_open
  -> spec_review
  -> spec_approved
  -> ready_to_implement

ready_to_implement
  -> impl_pr_open
  -> agent_review
  -> human_review
  -> ci_green
  -> merge_ready
  -> merged
  -> release_note_drafted
```

## Agent Boundary

Agents may:

- propose labels
- draft product and tech specs
- draft implementation plans
- perform first-pass PR review
- diagnose CI failures
- draft release notes

Agents must not:

- approve pull requests as final authority
- merge
- close disputed issues
- publish secrets or security findings
- change repository permissions
- bypass maintainer readiness labels

## Required Artifacts

Feature work that is not clearly small and actionable requires a spec packet:

```text
specs/GH<issue-number>/
  product.md
  tech.md
```

Bug fixes may skip the spec packet only when the issue has reproduction steps,
expected behavior, actual behavior, and an accepted `ready_to_implement` label.

## Verification

The first validator is intentionally deterministic:

- required files exist
- workflow labels are declared
- templates include required sections
- JSON schemas parse
- optional spec packets contain `product.md` and `tech.md`
- optional GitHub PR merge evidence can be collected read-only with
  `checks/github_pr_evidence.py`
- optional PR merge evidence can be checked with `checks/pr_gate.py`

LLM-based triage and review should be added only after the deterministic checks
are stable.
