from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from shutil import which
from unittest import SkipTest

import pytest


ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
PRE_COMMIT_HOOK = ROOT / ".githooks" / "pre-commit"
CI_RESULT_CHECK = ROOT / "scripts" / "check_ci_results.py"
WHITESPACE_CHECK = ROOT / "scripts" / "check_committed_whitespace.py"

CI_RESULT_ENV_PREFIX = "HARNESS_CI_RESULT_"
CI_RESULT_ENV_BINDINGS = {
    "HARNESS_CI_RESULT_CHANGED": "${{ needs.changed.result }}",
    "HARNESS_CI_RESULT_STORAGE_LEGACY_OPENERS": (
        "${{ needs.storage-legacy-openers.result }}"
    ),
    "HARNESS_CI_RESULT_REPOSITORY_CHECKS": "${{ needs.repository-checks.result }}",
    "HARNESS_CI_RESULT_FMT": "${{ needs.fmt.result }}",
    "HARNESS_CI_RESULT_WEB_BUILD": "${{ needs.web-build.result }}",
    "HARNESS_CI_RESULT_CLIPPY": "${{ needs.clippy.result }}",
    "HARNESS_CI_RESULT_TEST": "${{ needs.test.result }}",
    "HARNESS_CI_RESULT_AUDIT": "${{ needs.audit.result }}",
}

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


def job_mapping_value(block: str, mapping: str, key: str) -> str | None:
    lines = block.splitlines()
    for index, line in enumerate(lines):
        if line != f"    {mapping}:":
            continue
        for nested in lines[index + 1 :]:
            if nested.strip() and len(nested) - len(nested.lstrip()) <= 4:
                break
            prefix = f"      {key}:"
            if nested.startswith(prefix):
                return nested.removeprefix(prefix).strip().strip("\"'")
        return None
    return None


def step_blocks(block: str) -> list[str]:
    lines = block.splitlines()
    steps: list[str] = []
    current: list[str] | None = None
    in_steps = False

    for line in lines:
        if line == "    steps:":
            in_steps = True
            continue
        if not in_steps:
            continue
        if line.strip() and len(line) - len(line.lstrip()) <= 4:
            break
        if line.startswith("      - "):
            if current is not None:
                steps.append("\n".join(current))
            current = [line]
        elif current is not None:
            current.append(line)

    if current is not None:
        steps.append("\n".join(current))
    return steps


def step_value(step: str, key: str) -> str | None:
    prefixes = (f"      - {key}:", f"        {key}:")
    for line in step.splitlines():
        for prefix in prefixes:
            if line.startswith(prefix):
                return line.removeprefix(prefix).strip()
    return None


def step_mapping_values(step: str, mapping: str) -> dict[str, str]:
    lines = step.splitlines()
    for index, line in enumerate(lines):
        if line != f"        {mapping}:":
            continue

        values: dict[str, str] = {}
        for nested in lines[index + 1 :]:
            if nested.strip() and len(nested) - len(nested.lstrip()) <= 8:
                break
            match = re.fullmatch(r"\s{10}([A-Za-z0-9_-]+):\s*(.*?)\s*", nested)
            if match is None:
                continue
            key, value = match.groups()
            assert key not in values, f"duplicate {mapping} key in step: {key}"
            values[key] = value
        return values

    return {}


def named_steps(block: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for step in step_blocks(block):
        name = step_value(step, "name")
        if name is not None:
            assert name not in result, f"duplicate step name: {name}"
            result[name] = step
    return result


def assert_required_step(step: str) -> None:
    assert step_value(step, "if") is None, "required step must not be conditionally disabled"
    assert step_value(step, "continue-on-error") in {
        None,
        "false",
        '"false"',
        "'false'",
    }, "required step must fail the job on error"
    assert step_value(step, "shell") is None, "required step must use the default fail-fast shell"


def assert_exact_run_step(step: str, expected_command: str) -> None:
    assert_required_step(step)
    assert run_commands(step) == [expected_command], (
        f"required step must run exactly {expected_command!r}: {run_commands(step)}"
    )


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
    for script in (
        "scripts/check_ci_results.py",
        "scripts/check_committed_whitespace.py",
    ):
        assert f"- '{script}'" in changed_lines

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
        assert job_mapping_value(jobs[consumer], "env", "HARNESS_SKIP_WEB_BUILD") == "1"
        for step in step_blocks(jobs[consumer]):
            assert "HARNESS_SKIP_WEB_BUILD" not in step, (
                f"{consumer} step must not override HARNESS_SKIP_WEB_BUILD"
            )

    test_lines = all_runs["test"]
    assert "cargo test ${{ steps.scope.outputs.packages }}" in test_lines
    assert "cargo clippy --workspace --all-targets -- -D warnings" in all_runs["clippy"]
    for token in (
        "packages=--workspace --exclude harness-server",
        "-p harness-core -p harness-workflow",
        "-p harness-agents",
        "scripts/test-server-fast.sh",
        "scripts/test-server-db.sh",
    ):
        assert any(token in line for line in test_lines), f"test scope is missing: {token}"

    assert job_level_value(jobs["repository-checks"], "if") is None
    assert job_level_value(jobs["repository-checks"], "continue-on-error") is None
    assert job_level_value(jobs["repository-checks"], "defaults") is None
    repository_step_blocks = step_blocks(jobs["repository-checks"])
    repository_checkouts = [
        step
        for step in repository_step_blocks
        if step_value(step, "uses") == "actions/checkout@v4"
    ]
    assert len(repository_checkouts) == 1
    assert_required_step(repository_checkouts[0])
    assert step_mapping_values(repository_checkouts[0], "with") == {
        "fetch-depth": "0"
    }
    repository_steps = named_steps(jobs["repository-checks"])
    assert_exact_run_step(
        repository_steps["Test repository contracts"],
        "python3 -m pytest -q tests",
    )
    assert_exact_run_step(
        repository_steps["Check committed whitespace"],
        "python3 scripts/check_committed_whitespace.py",
    )

    fan_in = jobs["ci-result"]
    expected_needs = expected_jobs - {"ci-result"}
    assert job_needs(fan_in) == expected_needs
    assert job_level_value(fan_in, "if") in {"always()", "${{ always() }}"}
    assert job_level_value(fan_in, "continue-on-error") in {None, "false"}
    assert job_level_value(fan_in, "defaults") is None

    fan_in_step_blocks = step_blocks(fan_in)
    assert len(fan_in_step_blocks) == 2, "CI Result must only checkout and evaluate"
    checkout_step, fan_in_step = fan_in_step_blocks
    assert step_value(checkout_step, "uses") == "actions/checkout@v4"
    assert_required_step(checkout_step)
    assert step_mapping_values(checkout_step, "with") == {}
    assert_exact_run_step(fan_in_step, "python3 scripts/check_ci_results.py")
    assert step_value(fan_in_step, "name") == "Check all jobs"
    assert step_mapping_values(fan_in_step, "env") == CI_RESULT_ENV_BINDINGS

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
    ("updates", "removed", "extra", "arguments", "expected_code"),
    [
        ({}, None, None, [], 0),
        ({"HARNESS_CI_RESULT_TEST": "skipped"}, None, None, [], 0),
        ({"HARNESS_CI_RESULT_TEST": "failure"}, None, None, [], 1),
        ({"HARNESS_CI_RESULT_CLIPPY": "cancelled"}, None, None, [], 1),
        ({"HARNESS_CI_RESULT_FMT": "unknown"}, None, None, [], 2),
        ({}, "HARNESS_CI_RESULT_AUDIT", None, [], 2),
        ({}, None, ("HARNESS_CI_RESULT_BOGUS", "success"), [], 2),
        ({}, None, None, ["changed=success"], 2),
    ],
)
def test_ci_result_script_fails_closed(
    updates: dict[str, str],
    removed: str | None,
    extra: tuple[str, str] | None,
    arguments: list[str],
    expected_code: int,
) -> None:
    environment = {
        name: value
        for name, value in os.environ.items()
        if not name.startswith(CI_RESULT_ENV_PREFIX)
    }
    environment.update({name: "success" for name in CI_RESULT_ENV_BINDINGS})
    environment.update(updates)
    if removed is not None:
        environment.pop(removed)
    if extra is not None:
        environment[extra[0]] = extra[1]

    result = subprocess.run(
        [sys.executable, str(CI_RESULT_CHECK), *arguments],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode == expected_code, result.stdout + result.stderr


def run_git(repo: Path, *arguments: str) -> str:
    git = which("git")
    if git is None:
        raise SkipTest("git is required to validate committed whitespace")
    result = subprocess.run(
        [git, *arguments],
        cwd=repo,
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout.strip()


def commit_file(repo: Path, content: str, message: str) -> str:
    (repo / "sample.txt").write_text(content, encoding="utf-8")
    run_git(repo, "add", "sample.txt")
    run_git(repo, "commit", "-m", message)
    return run_git(repo, "rev-parse", "HEAD")


def initialize_git_repo(path: Path) -> None:
    path.mkdir()
    run_git(path, "init")
    run_git(path, "config", "user.email", "ci-contract@example.invalid")
    run_git(path, "config", "user.name", "CI Contract Test")
    run_git(path, "config", "commit.gpgSign", "false")
    run_git(path, "config", "core.hooksPath", ".git/hooks")


def create_git_repo(path: Path) -> tuple[str, str]:
    initialize_git_repo(path)
    base = commit_file(path, "clean\n", "base")
    head = commit_file(path, "trailing whitespace \n", "candidate")
    return base, head


def run_whitespace_check(
    repo: Path,
    event_path: Path,
    event_name: str,
    payload: dict[str, object],
) -> subprocess.CompletedProcess[str]:
    event_path.write_text(json.dumps(payload), encoding="utf-8")
    environment = os.environ.copy()
    environment.update(
        {
            "GITHUB_EVENT_NAME": event_name,
            "GITHUB_EVENT_PATH": str(event_path),
        }
    )
    return subprocess.run(
        [sys.executable, str(WHITESPACE_CHECK)],
        cwd=repo,
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )


@pytest.mark.parametrize("event_name", ["pull_request", "push", "workflow_dispatch"])
def test_whitespace_check_fails_on_committed_trailing_space(
    tmp_path: Path,
    event_name: str,
) -> None:
    repo = tmp_path / "repo"
    base, head = create_git_repo(repo)
    if event_name == "pull_request":
        payload: dict[str, object] = {
            "pull_request": {
                "base": {"sha": base},
                "head": {"sha": head},
            }
        }
    elif event_name == "push":
        payload = {"before": base}
    else:
        payload = {}

    result = run_whitespace_check(
        repo,
        tmp_path / f"{event_name}.json",
        event_name,
        payload,
    )

    assert result.returncode != 0, result.stdout + result.stderr
    assert "trailing whitespace" in result.stdout


def test_whitespace_check_passes_clean_pull_request(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    base = commit_file(repo, "clean\n", "base")
    head = commit_file(repo, "still clean\n", "candidate")
    payload: dict[str, object] = {
        "pull_request": {
            "base": {"sha": base},
            "head": {"sha": head},
        }
    }

    result = run_whitespace_check(
        repo,
        tmp_path / "pull-request.json",
        "pull_request",
        payload,
    )

    assert result.returncode == 0, result.stdout + result.stderr


def test_whitespace_check_fails_closed_without_event(tmp_path: Path) -> None:
    environment = os.environ.copy()
    environment.pop("GITHUB_EVENT_NAME", None)
    environment.pop("GITHUB_EVENT_PATH", None)
    result = subprocess.run(
        [sys.executable, str(WHITESPACE_CHECK)],
        cwd=tmp_path,
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )

    assert result.returncode == 2
    assert "GITHUB_EVENT_NAME is required" in result.stderr


def test_whitespace_check_fails_closed_without_pull_request_range(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    commit_file(repo, "clean\n", "base")

    result = run_whitespace_check(
        repo,
        tmp_path / "pull-request.json",
        "pull_request",
        {"pull_request": {}},
    )

    assert result.returncode == 2
    assert "base and head payloads are required" in result.stderr


@pytest.mark.parametrize(
    ("old", "new"),
    [
        (
            "needs: [changed, storage-legacy-openers, repository-checks, "
            "fmt, web-build, clippy, test, audit]",
            "needs: [changed, storage-legacy-openers, fmt, web-build, clippy, test, audit]",
        ),
        (
            "        run: python3 scripts/check_ci_results.py",
            "        run: python3 scripts/check_ci_results.py || true",
        ),
        (
            "        run: python3 scripts/check_ci_results.py",
            "        run: |\n          python3 scripts/check_ci_results.py\n          echo bypass",
        ),
        (
            "          HARNESS_CI_RESULT_AUDIT: ${{ needs.audit.result }}",
            "          HARNESS_CI_RESULT_BOGUS: ${{ needs.audit.result }}",
        ),
        (
            "        run: python3 scripts/check_ci_results.py",
            "        shell: bash {0} || true\n        run: python3 scripts/check_ci_results.py",
        ),
        (
            "      - uses: actions/checkout@v4\n"
            "      - name: Check all jobs",
            "      - uses: actions/checkout@v4\n"
            "        with:\n"
            "          path: source\n"
            "      - name: Check all jobs",
        ),
        ("          fetch-depth: 0", "          fetch-depth: 1"),
        (
            "- run: cargo test ${{ steps.scope.outputs.packages }}",
            "- run: echo tests-disabled",
        ),
        (
            "      - name: Test repository contracts\n        run: python3 -m pytest -q tests",
            "      - name: Test repository contracts\n"
            "        if: ${{ false }}\n"
            "        run: python3 -m pytest -q tests",
        ),
        (
            "      - name: Test repository contracts\n        run: python3 -m pytest -q tests",
            "      - name: Test repository contracts\n"
            "        run: |\n"
            "          if false; then\n"
            "            python3 -m pytest -q tests\n"
            "          fi",
        ),
        (
            "      - name: Check committed whitespace\n"
            "        run: python3 scripts/check_committed_whitespace.py",
            "      - name: Check committed whitespace\n"
            "        continue-on-error: true\n"
            "        run: python3 scripts/check_committed_whitespace.py",
        ),
        (
            "        run: python3 scripts/check_committed_whitespace.py",
            "        run: python3 scripts/check_committed_whitespace.py || true",
        ),
        (
            "      - run: cargo clippy --workspace --all-targets -- -D warnings",
            "      - run: cargo clippy --workspace --all-targets -- -D warnings\n"
            "        env:\n"
            '          HARNESS_SKIP_WEB_BUILD: "0"',
        ),
        (
            "      - run: cargo test ${{ steps.scope.outputs.packages }}\n"
            "        env:\n"
            "          HARNESS_DATABASE_URL:",
            "      - run: cargo test ${{ steps.scope.outputs.packages }}\n"
            "        env:\n"
            '          HARNESS_SKIP_WEB_BUILD: "0"\n'
            "          HARNESS_DATABASE_URL:",
        ),
        ('HARNESS_SKIP_WEB_BUILD: "1"', 'HARNESS_SKIP_WEB_BUILD: "0"'),
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
