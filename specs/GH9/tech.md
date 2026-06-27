# GH9 Tech Spec: GitHub PR Evidence Adapter

Product spec: `specs/GH9/product.md`
GitHub issue: `#9`

## Context

- `checks/pr_gate.py` evaluates local PR evidence JSON and intentionally avoids
  live GitHub calls.
- `skills/specrail-workflow/SKILL.md` tells agents to collect PR head SHA, CI,
  review threads, merge state, and human merge authorization before merge.
- `integrations/threads.md` assigns remote queue truth and closure audit to
  threads-style orchestration when available.
- `PLAN.md` names GitHub evidence adapters as the next phase after the local
  evaluator.

## Proposed Design

Add `checks/github_pr_evidence.py`.

CLI:

```sh
python3 checks/github_pr_evidence.py \
  --github-repo majiayu000/specrail \
  --pr 8 \
  --authorization-actor user \
  --authorization-source chat \
  --authorization-summary "merge approved" \
  --json
```

Adapter responsibilities:

- Validate `OWNER/REPO` and PR number inputs.
- Run `gh pr view <pr> --repo <owner/repo> --json ...`.
- Run `gh api graphql` for `reviewThreads(first:100)`.
- Normalize GitHub field names into `pr_gate.py` evidence:
  - `number` -> `pr`
  - `state` -> `state`
  - `isDraft` -> `is_draft`
  - `headRefOid` -> `head_sha`
  - `mergeStateStatus` -> `merge_state`
  - `closingIssuesReferences[0].number` -> `linked_issue`
  - `statusCheckRollup[]` -> `checks[]`
  - `reviews[]` -> `reviews[]`
  - `reviewThreads[]` -> `review_threads[]`
- Include `human_authorization` only when actor and source are both provided.

Rejected alternative:

- Do not add GitHub calls to `pr_gate.py`; doing so would make the policy gate
  network-dependent and harder to test offline.

## Test Plan

- Unit-test repository parser and evidence normalization.
- Unit-test optional human authorization handling.
- CLI test with a fake `gh` executable in `PATH`.
- Existing PR gate tests continue to validate policy behavior.

Commands:

```sh
python3 -m pytest tests/test_github_pr_evidence.py tests/test_pr_gate.py tests/test_evaluate.py
python3 checks/check_workflow.py --repo .
python3 checks/check_workflow.py --repo . --spec-dir specs/GH9
python3 -m compileall checks evaluate.py
```

## Rollback Plan

Remove `checks/github_pr_evidence.py`, its tests, docs/skill references, and the
GH9 spec packet. The adapter is read-only, so rollback has no remote side
effects.
