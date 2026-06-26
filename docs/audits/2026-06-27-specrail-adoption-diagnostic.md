# SpecRail Adoption Diagnostic

> Audit date: 2026-06-27
> Target: Harness repository
> Install worktree: `/Users/apple/Desktop/code/AI/tool/harness-specrail-workflow-design`
> Source pack: `majiayu000/specrail` at `main`
> Method: repository guidance review, stack detection, dirty-worktree boundary check, SpecRail pack validation, Python tests, and Rust compile check.

## Summary

| Severity | Count | Key areas |
|---|---:|---|
| Critical | 0 | None found in this adoption slice |
| High | 1 | Dirty source checkout must not receive broad workflow-pack writes |
| Medium | 3 | GitHub workflow adaptation, local Python toolchain health, process-contract overlap |
| Low | 1 | Example adoption records are intentionally advisory |

## Scope Boundary

The active source checkout was not used as the write target because it already
contained uncommitted work on `feat/usage-monitor-dashboard`. The SpecRail
adoption was installed in a clean worktree based on current `origin/main`.

## Findings

### H1: Source checkout is too dirty for broad workflow-pack writes

- Evidence: `git status --short --branch` in `/Users/apple/Desktop/code/AI/tool/harness` showed local modifications to high-context and startup/config files including `AGENTS.md`, `WORKFLOW.md`, `config/default.toml.example`, `docker-compose.yml`, `start-server.sh`, plus untracked evaluation docs and scripts.
- Fact: SpecRail installation adds root workflow files, checks, schemas, templates, policies, examples, tests, and a GitHub workflow. Applying that directly to the dirty checkout would mix unrelated work.
- Impact: Accidental commits could bundle user-local runtime/config work with workflow-pack adoption.
- Confidence: High.
- Suggested fix: Keep the adoption in the clean `codex/specrail-workflow-design` worktree and merge it independently.

### M1: SpecRail workflow must be validated as an adopted pack, not as a Harness feature runtime

- Evidence: `workflow.yaml`, `states.yaml`, `labels.yaml`, `templates/`, `schemas/`, `checks/`, and `skills/specrail-workflow/` are process-contract assets rather than Rust or web runtime code.
- Fact: The adoption installs deterministic workflow gates and examples. It does not alter Harness server behavior, agent adapters, or the web dashboard.
- Impact: Runtime acceptance should come from pack validators and Python tests first; Rust checks prove the added files did not break workspace compilation.
- Confidence: High.
- Suggested fix: Gate future changes with `python3 checks/check_workflow.py --repo .`, `python3 -m pytest -q tests`, and the normal Harness Rust checks when code changes are present.

### M2: The local Homebrew Python 3.14 toolchain has a broken `pyexpat` dependency

- Evidence: `python3 -m pip install --user pytest` failed while importing `pyexpat.cpython-314-darwin.so` because `_XML_SetAllocTrackerActivationThreshold` was missing from `/usr/lib/libexpat.1.dylib`.
- Fact: The failure is local toolchain health, not SpecRail or Harness code.
- Impact: Commands that need pip under Homebrew Python 3.14 may fail locally even though GitHub Actions installs dependencies on Ubuntu.
- Confidence: High.
- Suggested fix: Use `/usr/bin/python3` or another healthy Python for local SpecRail tests, or repair the Homebrew Python/libexpat installation separately.

### M3: SpecRail and Harness both define agent workflow conventions

- Evidence: Harness already has `skills/*.md` workflow skills and repo rules in `AGENTS.md`; SpecRail adds `AGENT_USAGE.md`, `workflow.yaml`, `states.yaml`, `labels.yaml`, and `skills/specrail-workflow/SKILL.md`.
- Fact: These contracts are compatible only if Harness repo rules remain higher priority and SpecRail is treated as the issue/spec/PR process layer.
- Impact: Agents could otherwise confuse Harness runtime concepts with SpecRail issue-state concepts.
- Confidence: Medium.
- Suggested fix: In agent prompts and future docs, load `AGENTS.md` first, then SpecRail files, and keep machine IDs such as `ready_to_spec` and `review_pr` distinct from Harness workflow runtime types.

### L1: Example adoption records are advisory and intentionally produce `needs_human`

- Evidence: `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json` returned `status=needs_human`, `errors=[]`, with warnings about `rclean` adoption needing human review.
- Fact: This is expected SpecRail example data, not a failed installation.
- Impact: CI should rely on `checks/check_workflow.py` and tests for pass/fail. `evaluate.py` can surface advisory status for examples.
- Confidence: High.
- Suggested fix: Keep the examples as reference material. Do not treat `needs_human` in adoption examples as a Harness release blocker.

## Installed Surface

- Root contract files: `AGENT_USAGE.md`, `SPEC.md`, `PLAN.md`, `workflow.yaml`, `states.yaml`, `labels.yaml`
- Deterministic checks: `checks/`, `evaluate.py`
- Machine contracts: `schemas/`
- Human and agent templates: `templates/`, `locales/`
- Review and policy docs: `review/`, `policies/`
- Optional integrations: `integrations/`
- Agent skill entrypoint: `skills/specrail-workflow/`
- Examples and smoke evidence: `examples/`, `specs/`
- CI workflow: `.github/workflows/workflow-check.yml`

Existing Harness `README.md`, `LICENSE`, and `CHANGELOG.md` were left intact.

## Verification

Commands run from the clean adoption worktree:

```sh
python3 checks/check_workflow.py --repo . --spec-dir specs/GH5 --spec-dir specs/GH7 --spec-dir specs/GH9 --spec-dir specs/GH13
/usr/bin/python3 evaluate.py --repo . --spec-dir specs/GH13 --format json
/usr/bin/python3 -m pytest -q tests
git diff --check
cargo check
```

Results:

- SpecRail check: passed.
- SpecRail evaluation: `needs_human` with no errors; warnings are from reference adoption records.
- Python tests: 18 passed.
- Rust compile check: passed.
- Whitespace check: passed.

## Repair Roadmap

| Phase | Scope | Dependencies | Validation |
|---|---|---|---|
| 1 | Land SpecRail pack as process-only adoption | Clean branch/worktree | SpecRail check, pytest, `cargo check` |
| 2 | Decide whether Harness should maintain its own adoption matrix entries | Maintainer decision | Update `examples/adoptions/matrix.json`, rerun `evaluate.py` |
| 3 | Add live GitHub label adoption if desired | Maintainer approval for labels | Dry-run label script or manual label review |
| 4 | Repair local Homebrew Python/libexpat | Local machine maintenance | `python3 -m pip --version`, `python3 -m pytest -q tests` |
