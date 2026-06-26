# Adoption Matrix and Fixture Validation - Product Spec

GitHub issue: `#13`
Locale: `en-US`
Route: `write_spec`

## Summary

GitHub issue `#13` turns previously executed real-repository pilots into a
SpecRail adoption matrix that can be displayed and checked from the repository.
Maintainers and agents should not need to infer adoption status from chat
history; they should be able to inspect committed docs and fixtures.

## Problem

1. `rclean`, `litellm-rs`, and `Claude-Code-Monitor` have each produced useful
   signals: read-only smoke coverage, PR gate coverage, and issue/spec/PR flow
   evidence. That evidence is currently spread across specs, tests, examples,
   and external GitHub state.
2. `evaluate.py` only checks the `rclean` smoke, so it cannot explain whether
   SpecRail has multi-repository adoption evidence.
3. Without a machine-readable adoption fixture, the matrix can drift into README
   copy and future changes can delete pilot evidence without failing tests.

## Goals

- Add `docs/ADOPTION_MATRIX.md` with adoption levels, current pilot repos,
  evidence paths, status, and next gaps.
- Add `examples/adoptions/matrix.json` with stable English keys for the same
  pilot set.
- Add schema and evaluator checks so `rclean`, `litellm-rs`, and
  `claude-code-monitor` cannot silently disappear.
- Include adoption matrix checks and artifacts in `evaluate.py` output.
- Keep all validation read-only and avoid modifying external pilot repos.

## Non-Goals

- Do not upgrade any external repository to full `repo_integrated` status in
  this issue.
- Do not add GitHub writes, automatic labels, automatic issue creation, or
  automatic merge behavior.
- Do not depend on chat history as runtime truth. Chat history is only a
  one-time source for migrating evidence.
- Do not add third-party Python dependencies.

## Users

- `maintainer`: needs to know which adoption level SpecRail has actually
  reached.
- `agent_worker`: needs existing pilot evidence and gaps before executing work.
- `reviewer`: needs deterministic checks proving the adoption matrix has not
  drifted.

## Behavior

### Adoption Levels

The matrix uses these stable levels:

- `referenced`
- `smoke`
- `spec_packet`
- `pr_gate`
- `repo_integrated`
- `automation_ready`

Each repository records only its strongest verified signal. A level is not a
maturity promise.

### Required Pilot Entries

The fixture contains:

- `rclean`: current level `smoke`
- `litellm-rs`: current level `pr_gate`
- `claude-code-monitor`: current level `spec_packet`

Each record contains:

- `id`
- `name`
- `repo`
- `current_level`
- `status`
- `evidence`
- `verified_behaviors`
- `next_gap`

### Evaluator

`python3 evaluate.py --repo . --spec-dir specs/GH13 --format json` verifies:

- workflow, spec, and task base artifacts
- `examples/rclean-smoke.md`
- `docs/ADOPTION_MATRIX.md`
- `examples/adoptions/matrix.json`
- SpecRail-local evidence paths
- external local path and GitHub URL evidence as recorded pointers only, with no
  network access and no writes to external repos

## Acceptance Criteria

- `docs/ADOPTION_MATRIX.md` exists and lists the pilot categories.
- `examples/adoptions/matrix.json` exists and contains the three required pilot
  IDs.
- `schemas/adoption_matrix.schema.json` exists and passes schema file checks.
- `evaluate.py` output contains adoption matrix artifacts and checks.
- Tests cover failure when a required pilot ID is missing.
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH13` passes.
- `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json` passes.
- `python3 -m pytest` passes.
