# SpecRail Evaluator and rclean Pilot Validation - Product Spec

GitHub issue: `#5`
Locale: `en-US`
Route: `write_spec`

## Summary

GitHub issue `#5` turns SpecRail from a documentation and template convention
into an executable evaluator. Maintainers and agents should be able to run the
same commands to verify that a SpecRail spec directory has the required
artifacts, task list, workflow constraints, and pilot evidence instead of
relying only on manual reading.

This issue also introduces `rclean` as a read-only adoption smoke test. `rclean`
is a real Rust CLI repository with clear CI commands and existing `docs/specs/`
material, but it lacks `AGENTS.md`, issue and PR templates, and the standard
SpecRail `specs/GH<number>/product.md` plus `tech.md` paths. The pilot must be
read-only and must not modify the `rclean` repository.

## Problem

1. SpecRail conventions mostly live in documents and templates, with no
   repeatable evaluator proving that a repo or spec satisfies the minimum
   workflow contract.
2. `check_workflow.py` needs to grow from basic pack checks into spec directory
   artifact checks, especially for `product.md`, `tech.md`, and `tasks.md`.
3. A top-level `evaluate.py` entrypoint is needed for agents, CI, and reviewers,
   with machine-readable output.
4. The pilot needs to cover common real-repo adoption risks: spec-first work,
   dangerous-operation gates, doc-only direct flow, CI command mapping, and
   duplicate issue avoidance.
5. Missing data must produce explicit failures. Missing artifacts, paths,
   commands, or evidence must not silently pass.

## Goals

- `check_workflow.py` verifies SpecRail repo config, required spec directory
  artifacts, task artifacts, and basic content completeness.
- `evaluate.py` combines workflow, spec, task, and smoke checks behind one
  stable evaluator CLI with stable exit codes.
- `tasks_artifact` is added at `specs/GH5/tasks.md` to track executable tasks,
  verification commands, and done-when criteria for issue `#5`.
- `examples/rclean-smoke.md` records the read-only `rclean` pilot facts,
  scenario matrix, and pass conditions.
- Agent-facing results are parseable, with stable IDs, paths, commands, and
  JSON keys kept in English.
- Reviewer-facing failures name what is missing, where it is missing, and what
  the next step should be.

## Non-Goals

- Do not modify the `rclean` repository, submit an `rclean` issue, create an
  `rclean` PR, or change its CI in this issue.
- Do not turn the evaluator into a general lint or test runner. It validates
  SpecRail workflow and adoption artifacts only.
- Do not replace human approval with evaluator output. Safety boundaries,
  force push, secrets, permission changes, and destructive actions still need
  human gates.
- Do not require existing repositories to immediately migrate old `docs/specs/`
  content. The `rclean` smoke records gaps and an adoption plan.
- Do not perform GitHub writes from the evaluator. Duplicate issue handling is
  limited to search/report evidence, not issue creation.

## Users

- `maintainer`: maintains SpecRail workflow, templates, schemas, and evaluator
  behavior.
- `agent_worker`: executes issue/spec tasks and needs to know the next step and
  missing artifacts.
- `reviewer`: reviews PRs and needs evidence that the SpecRail gate really ran.
- `pilot_repo_owner`: wants to try SpecRail without the pilot tool writing to
  their repository.

## Behavior

### `check_workflow.py`

1. Supports running from the repository root:

   ```sh
   python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
   ```

2. When `--spec-dir specs/GH5` is provided, it verifies:
   - `specs/GH5/product.md` exists.
   - `specs/GH5/tech.md` exists.
   - `specs/GH5/tasks.md` exists.
   - Artifact files are non-empty and include the related issue anchor `#5` or
     `GH5`.
   - Task IDs are unique and stable.

3. Missing required artifacts return failure. Unknown repository type, missing
   fields, or missing templates must not produce a pass result.

### `evaluate.py`

1. Provides a stable CLI:

   ```sh
   python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
   ```

2. `--format json` output includes these JSON keys:
   - `status`
   - `repo`
   - `spec_dir`
   - `checks`
   - `artifacts`
   - `errors`
   - `warnings`
   - `next_actions`

3. `status` is one of:
   - `pass`
   - `fail`
   - `needs_human`

4. Exit code semantics:
   - `0`: `status=pass` or `status=needs_human`; deterministic artifact checks
     did not fail, but a human gate may remain.
   - `1`: deterministic check failed.
   - `2`: CLI usage or config error.

5. The evaluator is read-only and does not modify the repository.

### `tasks_artifact`

1. Each SpecRail issue spec directory may include `tasks.md`.
2. For issue `#5`, `specs/GH5/tasks.md` is included as the implementation task
   plan; new spec packets may add it after `product.md` and `tech.md`.
3. Each task has a stable ID, owner scope, done-when criteria, and verification
   command or review proof.
4. Task status uses Markdown checkboxes for human and simple-parser access.

### `rclean` Smoke

`examples/rclean-smoke.md` covers these smoke scenarios:

- `rclean.new_rule_spec_first`: new rules require an issue/spec path before
  Rust rule code changes.
- `rclean.security_boundary_gate`: file deletion, path traversal, permission
  expansion, and secret-related work route to a human gate.
- `rclean.doc_only_direct`: small README/docs changes can use a direct doc-only
  flow with explicit verification.
- `rclean.ci_command_mapping`: Rust CI commands map to adoption evidence.
- `rclean.issue_dedupe`: when `drafts/rclean-issues-draft-2026-05-25.md`
  contains `NOT SUBMITTED YET`, the evaluator reports duplicate-issue risk
  instead of creating a new issue.

## Acceptance Criteria

- `specs/GH5/product.md`, `specs/GH5/tech.md`, and `specs/GH5/tasks.md` exist
  and reference one another.
- `examples/rclean-smoke.md` exists and marks the `rclean` pilot read-only.
- `evaluate.py` JSON output is stable, parseable, and points failures to
  concrete missing or invalid paths.
- `check_workflow.py` fails for duplicate task IDs when `tasks.md` exists,
  empty `product.md`, or empty `tech.md`.
- The evaluator does not execute destructive commands, write to the evaluated
  repository, or auto-submit issues/PRs.
- The `rclean` smoke can complete without modifying
  `/Users/lifcc/Desktop/code/AI/tool/rclean`.

## Done When

- This spec directory has all required artifacts.
- The evaluator implementation PR can run:

  ```sh
  python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
  python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
  python3 -m pytest tests/test_evaluate.py
  ```

- Reviewers can trace `issue #5`, `tasks_artifact`, and `rclean_smoke`
  pass/fail state from evaluator output.
