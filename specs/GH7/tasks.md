# GH7 Task Plan

## Linked Issue

GH-7

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [x] `SP7-T001` Owner: `workflow` | Done when: `checks/pr_gate.py` evaluates PR merge evidence and returns `allowed`, `needs_human`, or `blocked` | Verify: `python3 -m pytest tests/test_pr_gate.py`
- [x] `SP7-T002` Owner: `schema` | Done when: PR gate evidence fields are documented in schema/docs/templates | Verify: `python3 checks/check_workflow.py --repo .`
- [x] `SP7-T003` Owner: `docs` | Done when: agent usage and README mention the merge gate command and human-authorization boundary | Verify: documentation review plus `python3 checks/check_workflow.py --repo .`
- [x] `SP7-T004` Owner: `validation` | Done when: the GH7 spec packet validates and all focused tests pass | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH7`

## Parallelization

- `SP7-T001` owns `checks/pr_gate.py` and `tests/test_pr_gate.py`.
- `SP7-T002` and `SP7-T003` touch docs/schema/template files and should not run
  in parallel with each other.

## Verification

- `python3 -m pytest tests/test_pr_gate.py tests/test_evaluate.py` passed.
- `python3 checks/check_workflow.py --repo .` passed.
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH7` passed.
- `python3 -m compileall checks evaluate.py` passed.
- `git diff --check` passed.

## Handoff Notes

The first implementation should stay offline and evidence-file based. GitHub
API collection belongs in a later adapter.
