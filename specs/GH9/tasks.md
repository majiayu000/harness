# GH9 Task Plan

## Linked Issue

GH-9

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [x] `SP9-T001` Owner: `adapter` | Done when: `checks/github_pr_evidence.py` produces `pr_gate.py`-compatible evidence from GitHub-shaped payloads | Verify: `python3 -m pytest tests/test_github_pr_evidence.py`
- [x] `SP9-T002` Owner: `tests` | Done when: CLI behavior is tested with a fake `gh` executable and no network | Verify: `python3 -m pytest tests/test_github_pr_evidence.py`
- [x] `SP9-T003` Owner: `docs` | Done when: README, AGENT_USAGE, PLAN, and skill guidance describe the adapter boundary | Verify: `python3 checks/check_workflow.py --repo .`
- [x] `SP9-T004` Owner: `validation` | Done when: GH9 spec packet and focused tests pass | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH9`

## Parallelization

- `SP9-T001` and `SP9-T002` share adapter/test files and should stay serial.
- `SP9-T003` should run after adapter CLI names are stable.

## Verification

- `python3 -m pytest tests/test_github_pr_evidence.py tests/test_pr_gate.py tests/test_evaluate.py`
- `python3 checks/check_workflow.py --repo .`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH9`
- `python3 -m compileall checks evaluate.py`

## Handoff Notes

The adapter should remain read-only. Future work may add broader GitHub evidence
collection, but policy decisions should remain in `pr_gate.py`.
