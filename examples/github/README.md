# GitHub Consumer Example

This directory is a minimal GitHub setup for a repository that adopts a
SpecRail-style workflow without relying on hosted bots or a specific agent
runtime. Copy the files from this example into the root of a repository that
already carries the SpecRail workflow pack files and `checks/check_workflow.py`.

## Included Files

| Path | Purpose |
| --- | --- |
| `.github/ISSUE_TEMPLATE/specrail_feature.md` | Captures the problem, search evidence, acceptance criteria, and maintainer readiness state for issue-first work. |
| `.github/pull_request_template.md` | Records linked issue/spec packet, readiness checks, review checks, and verification evidence. |
| `.github/workflows/specrail-check.yml` | Runs read-only SpecRail validation and whitespace checks in GitHub Actions. |
| `specs/GH1/product.md` | Sample product spec layout for a linked GitHub issue. |
| `specs/GH1/tech.md` | Sample technical plan layout. |
| `specs/GH1/tasks.md` | Sample task checklist with stable task IDs. |

## How To Use

1. Copy `.github/`, `specs/`, and the SpecRail workflow pack files into the
   adopting repository.
2. Keep issue numbers aligned with spec packet directories, such as
   `specs/GH123/` for issue `#123`.
3. Keep the workflow read-only. It validates committed files and does not post
   comments, change labels, approve PRs, or call an agent runtime.
4. For each new numbered spec packet, run:

```bash
python3 checks/check_workflow.py --repo . --spec-dir specs/GH<number>
```

Run the pack-level check before opening a PR:

```bash
python3 checks/check_workflow.py --repo .
```
