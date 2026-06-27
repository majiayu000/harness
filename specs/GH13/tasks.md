# GH13 Tasks

Linked Issue: `#13`

Spec packet:

- Product: `specs/GH13/product.md`
- Tech: `specs/GH13/tech.md`

## Implementation Tasks

- [x] `SP13-T001` Owner: `docs` | Done when: `docs/ADOPTION_MATRIX.md` defines levels, current pilot repos, evidence, and next gaps | Verify: `test -s docs/ADOPTION_MATRIX.md`
- [x] `SP13-T002` Owner: `fixture` | Done when: `examples/adoptions/matrix.json` records `rclean`, `litellm-rs`, and `claude-code-monitor` | Verify: `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json`
- [x] `SP13-T003` Owner: `schema` | Done when: `schemas/adoption_matrix.schema.json` exists and the pack schema scan accepts it | Verify: `python3 checks/check_workflow.py --repo .`
- [x] `SP13-T004` Owner: `evaluator` | Done when: `evaluate.py` validates required adoption IDs and SpecRail-local evidence paths | Verify: `python3 -m pytest tests/test_evaluate.py`
- [x] `SP13-T005` Owner: `tests` | Done when: tests cover missing adoption pilot IDs and JSON contract artifacts | Verify: `python3 -m pytest tests/test_evaluate.py`
- [x] `SP13-T006` Owner: `validation` | Done when: pack, GH13 spec packet, evaluator, and tests all pass | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH13 && python3 evaluate.py --repo . --spec-dir specs/GH13 --format json && python3 -m pytest tests/test_evaluate.py tests/test_github_pr_evidence.py tests/test_pr_gate.py`

## Parallel Split

No parallel writable lanes are needed. All changes touch shared docs and
evaluator files, so a single writer owns the implementation.

## Verification

- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH13`
- `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json`
- `python3 -m pytest tests/test_evaluate.py tests/test_github_pr_evidence.py tests/test_pr_gate.py`

## Handoff Notes

The adoption matrix intentionally records current evidence levels. It does not
claim `repo_integrated` or `automation_ready` for any listed repository.
