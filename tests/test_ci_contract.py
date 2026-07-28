from __future__ import annotations

import re
import subprocess
from pathlib import Path
from shutil import which
from unittest import SkipTest

import pytest


ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
PRE_COMMIT_HOOK = ROOT / ".githooks" / "pre-commit"

JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):(?:\s+#.*)?$")
RUN_KEY = re.compile(r"^(\s*)(?:-\s+)?run:\s*(.*?)\s*$")
USES_KEY = re.compile(r"^\s+-\s+uses:\s*(\S+)\s*$")


def top_level_job_blocks(workflow: str) -> dict[str, str]:
    lines = workflow.splitlines()
    try:
        jobs_index = lines.index("jobs:")
    except ValueError as error:
        raise AssertionError("CI workflow is missing a top-level jobs mapping") from error

    jobs: dict[str, str] = {}
    current_name: str | None = None
    current_lines: list[str] = []

    def finish_current() -> None:
        if current_name is not None:
            jobs[current_name] = "\n".join(current_lines)

    for line in lines[jobs_index + 1 :]:
        stripped = line.strip()
        if stripped and not stripped.startswith("#") and not line.startswith(" "):
            break

        header = JOB_HEADER.fullmatch(line)
        if header is not None:
            finish_current()
            current_name = header.group(1)
            current_lines = []
        elif current_name is not None:
            current_lines.append(line)

    finish_current()
    return jobs


def job_level_value(block: str, key: str) -> str | None:
    prefix = f"    {key}:"
    for line in block.splitlines():
        if line.startswith(prefix):
            return line.removeprefix(prefix).strip()
    return None


def job_needs(block: str) -> set[str]:
    lines = block.splitlines()
    for index, line in enumerate(lines):
        if not line.startswith("    needs:"):
            continue

        value = line.removeprefix("    needs:").strip()
        if value.startswith("[") and value.endswith("]"):
            return {item.strip() for item in value[1:-1].split(",") if item.strip()}
        if value:
            return {value}

        needs: set[str] = set()
        for nested in lines[index + 1 :]:
            if nested.strip() and len(nested) - len(nested.lstrip()) <= 4:
                break
            match = re.fullmatch(r"\s{6}-\s+([A-Za-z0-9_-]+)\s*", nested)
            if match is not None:
                needs.add(match.group(1))
        return needs

    return set()


def run_commands(block: str) -> list[str]:
    lines = block.splitlines()
    commands: list[str] = []
    index = 0

    while index < len(lines):
        line = lines[index]
        if line.lstrip().startswith("#"):
            index += 1
            continue

        match = RUN_KEY.fullmatch(line)
        if match is None:
            index += 1
            continue

        indent = len(match.group(1))
        value = match.group(2)
        if value not in {"|", "|-", ">", ">-"}:
            commands.append(value)
            index += 1
            continue

        block_lines: list[str] = []
        index += 1
        while index < len(lines):
            nested = lines[index]
            nested_indent = len(nested) - len(nested.lstrip())
            if nested.strip() and nested_indent <= indent:
                break
            block_lines.append(nested.strip())
            index += 1
        commands.append("\n".join(block_lines))

    return commands


def active_run_lines(block: str) -> list[str]:
    return [
        line.strip()
        for command in run_commands(block)
        for line in command.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def action_uses(block: str) -> list[str]:
    return [
        match.group(1)
        for line in block.splitlines()
        if not line.lstrip().startswith("#")
        if (match := USES_KEY.fullmatch(line)) is not None
    ]


def assert_ci_contract(workflow: str, hook: str) -> None:
    jobs = top_level_job_blocks(workflow)
    expected_jobs = {
        "changed",
        "storage-legacy-openers",
        "repository-checks",
        "fmt",
        "web-build",
        "clippy",
        "test",
        "audit",
        "ci-result",
    }
    assert set(jobs) == expected_jobs, (
        f"unexpected CI jobs: missing={sorted(expected_jobs - jobs.keys())}, "
        f"extra={sorted(jobs.keys() - expected_jobs)}"
    )

    changed_lines = {
        line.strip()
        for line in jobs["changed"].splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    for output in ("workspace", "other_crates"):
        expected = f"{output}: ${{{{ steps.filter.outputs.{output} }}}}"
        assert expected in changed_lines, f"changed job is missing output mapping: {expected}"

    all_runs = {
        name: active_run_lines(block)
        for name, block in jobs.items()
    }
    bun_builds = [
        name
        for name, lines in all_runs.items()
        for line in lines
        if line == "bun run build"
    ]
    assert bun_builds == ["web-build"], f"web bundle builds must be isolated: {bun_builds}"

    assert action_uses(jobs["web-build"]).count("actions/upload-artifact@v4") == 1
    for consumer in ("clippy", "test"):
        assert action_uses(jobs[consumer]).count("actions/download-artifact@v4") == 1
        assert job_needs(jobs[consumer]) == {"changed", "web-build"}

    test_lines = all_runs["test"]
    assert "cargo test ${{ steps.scope.outputs.packages }}" in test_lines
    for token in (
        "packages=--workspace --exclude harness-server",
        "-p harness-core -p harness-workflow",
        "-p harness-agents",
        "scripts/test-server-fast.sh",
        "scripts/test-server-db.sh",
    ):
        assert any(token in line for line in test_lines), f"test scope is missing: {token}"

    repository_lines = all_runs["repository-checks"]
    assert "python3 -m pytest -q tests" in repository_lines
    diff_checks = [line for line in repository_lines if line.startswith("git diff --check")]
    assert len(diff_checks) == 4, f"expected four active whitespace checks, got {diff_checks}"

    fan_in = jobs["ci-result"]
    expected_needs = expected_jobs - {"ci-result"}
    assert job_needs(fan_in) == expected_needs
    assert job_level_value(fan_in, "if") in {"always()", "${{ always() }}"}

    fan_in_commands = "\n".join(run_commands(fan_in))
    result_refs = set(
        re.findall(
            r"\$\{\{\s*needs\.([A-Za-z0-9_-]+)\.result\s*\}\}",
            fan_in_commands,
        )
    )
    assert result_refs == expected_needs
    fan_in_lines = active_run_lines(fan_in)
    assert 'if [[ "$r" == "failure" || "$r" == "cancelled" ]]; then' in fan_in_lines
    assert "exit 1" in fan_in_lines

    hook_lines = {
        line.strip()
        for line in hook.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    for line in (
        "derive_scope() {",
        "staged=$(git diff --cached --name-only)",
        'echo "--workspace"',
        "scope=$(derive_scope)",
        "cargo clippy $scope --all-targets -- -D warnings",
    ):
        assert line in hook_lines, f"pre-commit hook is missing active line: {line}"


def test_job_parser_accepts_comments_and_block_needs() -> None:
    workflow = """\
jobs:
  first:
    runs-on: ubuntu-latest

# Keep the next job in the jobs mapping.
  second:
    needs:
      - first
    runs-on: ubuntu-latest
"""

    jobs = top_level_job_blocks(workflow)
    assert set(jobs) == {"first", "second"}
    assert job_needs(jobs["second"]) == {"first"}


def test_scoped_ci_pipeline_contract() -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")
    assert_ci_contract(workflow, hook)


@pytest.mark.parametrize(
    ("old", "new"),
    [
        (
            "needs: [changed, storage-legacy-openers, repository-checks, fmt, web-build, clippy, test, audit]",
            "needs: [changed, storage-legacy-openers, fmt, web-build, clippy, test, audit]",
        ),
        (
            'if [[ "$r" == "failure" || "$r" == "cancelled" ]]; then',
            "if false; then",
        ),
        (
            "- run: cargo test ${{ steps.scope.outputs.packages }}",
            "- run: echo tests-disabled",
        ),
        (
            "run: python3 -m pytest -q tests",
            "run: true\n      # run: python3 -m pytest -q tests",
        ),
        ("git diff --check", "echo whitespace-check-disabled"),
    ],
)
def test_ci_contract_rejects_silent_regressions(old: str, new: str) -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")
    mutated = workflow.replace(old, new)
    assert mutated != workflow, f"mutation target is missing: {old}"

    with pytest.raises(AssertionError):
        assert_ci_contract(mutated, hook)


def test_pre_commit_hook_syntax() -> None:
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
