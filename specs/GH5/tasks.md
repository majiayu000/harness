# GH5 Tasks

Issue: `#5`
Product spec: `specs/GH5/product.md`
Tech spec: `specs/GH5/tech.md`
Smoke example: `examples/rclean-smoke.md`

## Status

- [x] `SP5-T001` Owner: `docs` | Done when: `specs/GH5/product.md` defines evaluator goals, non-goals, required behavior, and acceptance criteria | Verify: `test -s specs/GH5/product.md`
- [x] `SP5-T002` Owner: `docs` | Done when: `specs/GH5/tech.md` defines CLI contract, JSON result envelope, check IDs, task parser rules, and test plan | Verify: `test -s specs/GH5/tech.md`
- [x] `SP5-T003` Owner: `docs` | Done when: `specs/GH5/tasks.md` exists with unique stable task IDs and done-when criteria | Verify: `test -s specs/GH5/tasks.md`
- [x] `SP5-T004` Owner: `pilot-docs` | Done when: `examples/rclean-smoke.md` records the read-only `rclean` scout facts and required smoke scenario IDs | Verify: `test -s examples/rclean-smoke.md`
- [x] `SP5-T005` Owner: `workflow` | Done when: `checks/check_workflow.py` validates `product.md`, `tech.md`, `tasks.md`, issue anchors, task IDs, and missing artifact failures for `specs/GH5` | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH5`
- [x] `SP5-T006` Owner: `evaluator` | Done when: `evaluate.py` supports `--repo`, `--spec-dir`, `--format json`, stable exit codes, and required JSON keys | Verify: `python3 evaluate.py --repo . --spec-dir specs/GH5 --format json`
- [x] `SP5-T007` Owner: `schema-template` | Done when: schemas/templates document `tasks_artifact` and keep stable JSON keys in English | Verify: `python3 checks/check_workflow.py --repo . --spec-dir specs/GH5`
- [x] `SP5-T008` Owner: `tests` | Done when: tests cover duplicate task IDs, missing `Done when:`, missing `Verify:`, missing `tasks.md`, and missing `rclean` smoke scenario IDs | Verify: `python3 -m pytest tests/test_evaluate.py`
- [x] `SP5-T009` Owner: `pilot` | Done when: evaluator can assess `examples/rclean-smoke.md` without writing to `/Users/lifcc/Desktop/code/AI/tool/rclean` | Verify: `python3 evaluate.py --repo . --spec-dir specs/GH5 --format json`
- [x] `SP5-T010` Owner: `review` | Done when: reviewer sees `status`, `checks`, `artifacts`, `errors`, `warnings`, and `next_actions` in evaluator output and can trace failures to concrete paths | Verify: `python3 evaluate.py --repo . --spec-dir specs/GH5 --format json`

## Current Priority

1. Finish the workflow gate so `tasks_artifact` becomes enforceable.
2. Finish the evaluator CLI so CI and agent workers have one entrypoint.
3. Finish parser and CLI tests before claiming issue `#5` complete.
