# Adoption matrix and fixture validation - Tech Spec

GitHub issue: `#13`
Product spec: `specs/GH13/product.md`
Tasks artifact: `specs/GH13/tasks.md`

## Current System

SpecRail already has these adoption signals:

- `examples/rclean-smoke.md` and `specs/GH5/*` describe a read-only `rclean`
  adoption smoke.
- `specs/GH7/*` and `tests/test_pr_gate.py` encode PR merge gate behavior
  informed by `rclean` and `litellm-rs`.
- `Claude-Code-Monitor` / `claude-hub` has an external GH44 spec packet and
  merged PR45.

`evaluate.py` currently validates workflow config, one spec packet, and the
`rclean` smoke. It does not validate multi-repo adoption records.

## Design

Add three layers:

1. Human-facing matrix: `docs/ADOPTION_MATRIX.md`.
2. Machine-facing fixture: `examples/adoptions/matrix.json`.
3. Deterministic evaluator checks in `evaluate.py`.

The fixture stays JSON to avoid adding PyYAML or another parser dependency.

## Data Flow

Input:

- `docs/ADOPTION_MATRIX.md`
- `examples/adoptions/matrix.json`
- SpecRail-local evidence paths named inside the fixture

Output:

- Additional `checks[]` from `evaluate.py`
- Additional `artifacts.adoption_matrix` and `artifacts.adoption_fixture`
- `errors[]` when required records or local evidence paths are missing

The evaluator does not call GitHub and does not inspect or mutate external
local repos. External paths and GitHub URLs are recorded as evidence pointers.

## Implementation Plan

1. Extend `check_workflow.py` required pack files with:
   - `docs/ADOPTION_MATRIX.md`
   - `examples/adoptions/matrix.json`
   - `schemas/adoption_matrix.schema.json`
2. Add `evaluate_adoption_matrix(repo)` to `evaluate.py`.
3. Require known IDs:
   - `rclean`
   - `litellm-rs`
   - `claude-code-monitor`
4. Validate each known record has a valid level, status, evidence, verified
   behaviors, and next gap.
5. Validate `specrail_artifact` paths exist under the SpecRail repo.
6. Add unit tests for missing pilot IDs and JSON contract coverage.

## Alternatives

- YAML fixture: rejected for now because the pack intentionally avoids
  third-party Python dependencies.
- Docs-only matrix: rejected because it would regress silently.
- Live GitHub validation: rejected for this issue because it would make the
  evaluator network-dependent.

## Risks

- Security: External local paths must remain read-only evidence pointers.
- Compatibility: Existing `evaluate.py` status remains `needs_human` because
  `rclean` smoke still intentionally requires human dedupe review.
- Performance: JSON parsing and path checks are trivial.
- Maintenance: Required pilot IDs should stay small until more repos have
  repeated real runs.

## Test Plan

- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH13`
- `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json`
- `python3 -m pytest tests/test_evaluate.py tests/test_github_pr_evidence.py tests/test_pr_gate.py`

## Rollback Plan

Remove the new docs, fixture, schema, evaluator checks, tests, and GH13 spec
packet. `evaluate.py` will return to only checking workflow/spec/rclean smoke.
