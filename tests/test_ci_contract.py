from __future__ import annotations

import subprocess
from pathlib import Path
from shutil import which
from unittest import SkipTest


ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
PRE_COMMIT_HOOK = ROOT / ".githooks" / "pre-commit"


def top_level_jobs(workflow: str) -> set[str]:
    jobs: set[str] = set()
    in_jobs = False

    for line in workflow.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if line == "jobs:":
            in_jobs = True
            continue
        if in_jobs and not line.startswith(" "):
            break
        if in_jobs and line.startswith("  ") and not line.startswith("    "):
            if stripped.endswith(":"):
                jobs.add(stripped.removesuffix(":"))

    return jobs


def test_top_level_jobs_ignores_comments_and_blank_lines() -> None:
    workflow = """\
jobs:
  first:
    runs-on: ubuntu-latest

# Keep the next job in the jobs mapping.
  second:
    runs-on: ubuntu-latest
"""

    assert top_level_jobs(workflow) == {"first", "second"}


def test_scoped_ci_pipeline_contract() -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    jobs = top_level_jobs(workflow)
    required_jobs = {
        "changed",
        "repository-checks",
        "fmt",
        "web-build",
        "clippy",
        "test",
        "audit",
        "ci-result",
    }

    assert required_jobs <= jobs, f"missing CI jobs: {sorted(required_jobs - jobs)}"
    assert "check" not in jobs, "legacy monolithic check job still exists"
    assert workflow.count("bun run build") == 1, "web bundle must be built exactly once"

    required_expressions = {
        "steps.filter.outputs.workspace",
        "steps.filter.outputs.other_crates",
        "Compute test scope",
        "steps.scope.outputs.packages",
        "actions/upload-artifact@v4",
        "actions/download-artifact@v4",
        "needs.web-build.result",
        "needs.repository-checks.result",
    }
    missing = sorted(expression for expression in required_expressions if expression not in workflow)
    assert not missing, f"missing CI contract expressions: {missing}"


def test_pre_commit_hook_contract_and_syntax() -> None:
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")

    for command in ("git diff --cached --name-only", "cargo clippy $scope"):
        assert command in hook, f"pre-commit hook is missing: {command}"

    bash = which("bash")
    if bash is None:
        raise SkipTest("bash is required to validate the pre-commit hook syntax")

    result = subprocess.run(
        [bash, "-n", str(PRE_COMMIT_HOOK)],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
