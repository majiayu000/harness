# GH7 Tech Spec: Deterministic PR Merge Gate

Product spec: `specs/GH7/product.md`
GitHub issue: `#7`

## Context

- `checks/route_gate.py` evaluates whether an action may start from local
  evidence.
- `checks/check_workflow.py` validates the workflow pack and spec packet shape.
- `schemas/pr_review_gate.schema.json` defines a PR review gate artifact, but
  there is no executable merge-readiness check.
- `integrations/threads.md` describes closure audit behavior, but keeps threads
  optional.
- The `litellm-rs` pilot showed the missing deterministic gate: merge required
  checking PR head SHA, CI, review thread resolution, merge state, and explicit
  user authorization.

## Proposed Design

Add `checks/pr_gate.py`.

Input:

```json
{
  "pr": 718,
  "state": "OPEN",
  "is_draft": false,
  "head_sha": "e36d975...",
  "merge_state": "CLEAN",
  "linked_issue": 716,
  "checks": [
    {"name": "workflow-check", "status": "COMPLETED", "conclusion": "SUCCESS"}
  ],
  "reviews": [
    {"author": "reviewer", "state": "COMMENTED"}
  ],
  "review_threads": [
    {"url": "https://example.invalid/thread", "is_resolved": true, "is_outdated": false}
  ],
  "human_authorization": {
    "actor": "maintainer",
    "source": "chat",
    "summary": "merge approved"
  }
}
```

Output:

```json
{
  "decision": "allowed",
  "reasons": [],
  "satisfied": [],
  "missing": [],
  "blocked_actions": []
}
```

Decision rules:

- `blocked`: deterministic merge safety failed.
- `needs_human`: deterministic safety passed but human authorization is absent.
- `allowed`: deterministic safety passed and human merge authorization is
  present.

## Test Plan

- Unit tests call the evaluator directly.
- CLI smoke uses fixture JSON from a temporary file.
- Pack validation checks the new script and docs tokens.

Commands:

```sh
python3 -m pytest tests/test_pr_gate.py tests/test_evaluate.py
python3 checks/check_workflow.py --repo .
python3 checks/check_workflow.py --repo . --spec-dir specs/GH7
python3 checks/pr_gate.py --repo . --evidence <fixture>
```

## Rollback Plan

Remove `checks/pr_gate.py`, its tests, docs/template references, and the GH7
spec packet. Because the gate is read-only and not wired to automatic writes,
rollback has no remote side effects.
