# SpecRail evaluator and rclean pilot validation - Tech Spec

Product spec: `specs/GH5/product.md`
Tasks artifact: `specs/GH5/tasks.md`
Smoke example: `examples/rclean-smoke.md`
GitHub issue: `#5`

## Context

Issue `#5` introduces an executable SpecRail evaluator. The intended implementation work is in `checks/*.py`, `workflow.yaml`, `schemas/templates`, and a new root `evaluate.py`; this document defines the contract for that implementation but does not modify those files.

The pilot target is `rclean`:

- repo: `/Users/lifcc/Desktop/code/AI/tool/rclean`
- origin: `https://github.com/majiayu000/rclean.git`
- branch: `feat/rules-python-global`
- untracked: `drafts/`
- CI commands:
  - `cargo fmt -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test`
  - `cargo build --release`
  - MSRV `1.95` build/test
- current context gaps:
  - no `AGENTS.md`
  - no `CLAUDE.md`
  - no `WARP.md`
  - no `.agents/skills`
  - no issue template
  - no PR template
  - old specs live under `docs/specs/`
  - draft issue file exists at `drafts/rclean-issues-draft-2026-05-25.md` and is marked `NOT SUBMITTED YET`

## Design

### Components

`checks/check_workflow.py`

- Owns deterministic validation helpers for workflow/spec/task artifacts.
- Must be importable by `evaluate.py`; avoid only-shelling-out to reuse logic.
- May still be executable as a standalone CLI for focused workflow checks.

`evaluate.py`

- Top-level evaluator CLI.
- Loads repo/spec inputs, runs validation checks, aggregates results, and emits a stable result envelope.
- Must be read-only by default.

`specs/GH5/tasks.md`

- First required `tasks_artifact` example.
- Serves both as issue task tracking and as parser fixture for unique stable IDs.

`examples/rclean-smoke.md`

- Read-only adoption smoke definition.
- Serves as a human-readable fixture for the first external repo validation.

### CLI Contract

Workflow check:

```sh
python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
```

Evaluator:

```sh
python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
```

Optional human format:

```sh
python3 evaluate.py --repo . --spec-dir specs/GH5 --format text
```

Required args:

- `--repo`: path to the SpecRail repo root.
- `--spec-dir`: path to the issue spec directory, relative to `--repo` or absolute.

Supported `--format` values:

- `json`
- `text`

Exit codes:

- `0`: deterministic artifact checks passed. The result may still be `needs_human`
  when a human gate is required.
- `1`: at least one deterministic check failed.
- `2`: invalid CLI args, unreadable repo root, invalid config, or internal evaluator usage error.

### Result Envelope

For `--format json`, `evaluate.py` returns one JSON object:

```json
{
  "status": "fail",
  "repo": ".",
  "spec_dir": "specs/GH5",
  "checks": [
    {
      "id": "spec.product_present",
      "status": "pass",
      "path": "specs/GH5/product.md",
      "message": "product spec exists"
    }
  ],
  "artifacts": {
    "product_spec": "specs/GH5/product.md",
    "tech_spec": "specs/GH5/tech.md",
    "tasks_artifact": "specs/GH5/tasks.md",
    "smoke_example": "examples/rclean-smoke.md"
  },
  "errors": [],
  "warnings": [],
  "next_actions": []
}
```

Allowed top-level `status` values:

- `pass`
- `fail`
- `needs_human`

Allowed per-check `status` values:

- `pass`
- `fail`
- `skip`
- `needs_human`

### Check IDs

Minimum required check IDs for issue `#5`:

- `workflow.config_present`
- `workflow.states_present`
- `workflow.labels_present`
- `spec.product_present`
- `spec.tech_present`
- `spec.tasks_present`
- `spec.issue_anchor_present`
- `tasks.ids_unique`
- `tasks.done_when_present`
- `tasks.verification_present`
- `rclean_smoke.present`
- `rclean_smoke.read_only`
- `rclean_smoke.scenarios_present`
- `rclean_smoke.ci_commands_present`
- `rclean_smoke.issue_dedupe_present`

If a repository intentionally does not have `states.yaml` or `labels.yaml`, the implementation may report `skip` only when `workflow.yaml` declares that omission explicitly. Silent omission is `fail`.

## `tasks.md` Format

The parser should accept Markdown checkboxes with stable IDs in backticks:

```md
- [ ] `SP5-T001` Owner: `evaluator` | Done when: evaluator returns JSON | Verify: `python3 evaluate.py --repo . --spec-dir specs/GH5 --format json`
```

Required fields:

- stable task ID, matching `SP5-T[0-9]+`
- checkbox status
- `Owner:`
- `Done when:`
- `Verify:`

Parser rules:

- Duplicate task IDs are `fail`.
- Missing `Done when:` is `fail`.
- Missing `Verify:` is `fail` unless the task is explicitly review-only with `Verify: review`.
- Unknown extra fields are allowed but should not affect pass/fail.

## `rclean` Smoke Validation

The evaluator does not need to execute `cargo` commands against `rclean`. It should verify that `examples/rclean-smoke.md` records:

- repo path and origin
- current branch
- untracked `drafts/`
- all CI commands from the scout summary
- absence of repo-level agent context files
- old specs path `docs/specs/`
- draft issue file and `NOT SUBMITTED YET` marker
- five required smoke scenario IDs

Required smoke scenario IDs:

- `rclean.new_rule_spec_first`
- `rclean.security_boundary_gate`
- `rclean.doc_only_direct`
- `rclean.ci_command_mapping`
- `rclean.issue_dedupe`

## Error Handling

- Use explicit errors for missing files, unreadable paths, malformed task IDs, duplicate task IDs, and missing required smoke scenarios.
- Do not downgrade user-visible missing data to warning.
- Do not infer paths that are not declared in inputs or config.
- Do not catch broad exceptions and continue as pass.
- If a parser cannot understand a file, the check result is `fail` with the parser error message.

## Test Plan

Unit tests:

- `tasks.ids_unique` fails on duplicate IDs.
- `tasks.done_when_present` fails when `Done when:` is missing.
- `tasks.verification_present` fails when `Verify:` is missing.
- `spec.tasks_present` fails when `specs/GH5/tasks.md` is missing.
- `rclean_smoke.scenarios_present` fails when one required scenario ID is absent.
- `evaluate.py --format json` returns valid JSON with required top-level keys.

CLI validation:

```sh
python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
```

Smoke review:

```sh
sed -n '1,220p' examples/rclean-smoke.md
```

## Risks and Mitigations

Risk: evaluator scope grows into a generic CI runner.

Mitigation: keep `evaluate.py` focused on SpecRail artifacts and read-only adoption evidence. CI commands are mapped as evidence, not executed.

Risk: `tasks.md` parsing becomes brittle.

Mitigation: require only checkbox, stable ID, `Owner:`, `Done when:`, and `Verify:` fields. Treat additional fields as opaque text.

Risk: pilot smoke accidentally writes to `rclean`.

Mitigation: `rclean_smoke.read_only` is a required check, and smoke commands are limited to inspection commands unless a human explicitly starts a separate implementation task in the `rclean` repo.

Risk: missing workflow files are incorrectly treated as optional.

Mitigation: missing `workflow.yaml`, `states.yaml`, or `labels.yaml` fails unless an explicit config declaration permits `skip`.

## Handoff Notes

- This doc worker only owns `specs/GH5/product.md`, `specs/GH5/tech.md`, `specs/GH5/tasks.md`, and `examples/rclean-smoke.md`.
- Implementation workers own `checks/*.py`, `workflow.yaml`, `schemas/templates`, and `evaluate.py`.
- Do not revert unrelated local changes while landing issue `#5`.
