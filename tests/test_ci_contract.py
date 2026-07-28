from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path
from shutil import which
from unittest import SkipTest

import pytest
from ci_contract_support import (
    CI_RESULT_ENV,
    PYTEST_CONFIG_BAITS,
    PYTEST_CONFTEST_HOOKS,
    PYTEST_ROOT_BAITS,
    REPOSITORY_PYTEST_COMMAND,
    assert_ci_contract,
    assert_pytest_attack_blocked,
    commit_files,
    create_autoloading_pytest_plugin,
    create_git_repo,
    create_pytest_canary,
    initialize_git_repo,
    parse_workflow,
    run_git,
    run_whitespace_check,
)


ROOT = Path(__file__).resolve().parents[1]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
PRE_COMMIT_HOOK = ROOT / ".githooks" / "pre-commit"
CI_RESULT_CHECK = ROOT / "scripts" / "check_ci_results.py"
WHITESPACE_CHECK = ROOT / "scripts" / "check_committed_whitespace.py"

CI_RESULT_ENV_PREFIX = "HARNESS_CI_RESULT_"
ZERO_OBJECT_ID = "0" * 40
UNREACHABLE_OBJECT_ID = "f" * 40


def test_restricted_yaml_parser_accepts_comments_and_block_sequences() -> None:
    parsed = parse_workflow(
        """\
jobs:
  first:
    runs-on: ubuntu-latest

# Keep the next job in the jobs mapping.
  second:
    needs:
      - first
    runs-on: ubuntu-latest
"""
    )

    assert parsed == {
        "jobs": {
            "first": {"runs-on": "ubuntu-latest"},
            "second": {
                "needs": ["first"],
                "runs-on": "ubuntu-latest",
            },
        }
    }


def test_scoped_ci_pipeline_contract() -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")
    assert_ci_contract(workflow, hook)


@pytest.mark.parametrize(
    ("updates", "removed", "extra", "arguments", "expected_code"),
    [
        ({}, None, None, [], 0),
        ({"HARNESS_CI_RESULT_TEST": "skipped"}, None, None, [], 0),
        ({"HARNESS_CI_RESULT_AUDIT": "skipped"}, None, None, [], 0),
        ({"HARNESS_CI_RESULT_CHANGED": "skipped"}, None, None, [], 1),
        ({"HARNESS_CI_RESULT_REPOSITORY_CHECKS": "skipped"}, None, None, [], 1),
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
    environment.update({name: "success" for name in CI_RESULT_ENV})
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


@pytest.mark.parametrize(
    ("relative", "content"),
    PYTEST_CONFIG_BAITS,
)
def test_hermetic_pytest_ignores_repository_config(
    tmp_path: Path,
    relative: str,
    content: str,
) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    (project / relative).write_text(content, encoding="utf-8")
    assert_pytest_attack_blocked(project)


@pytest.mark.parametrize(
    ("relative", "content"),
    PYTEST_ROOT_BAITS,
)
def test_hermetic_pytest_ignores_root_python_bait(
    tmp_path: Path,
    relative: str,
    content: str,
) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    (project / relative).write_text(content, encoding="utf-8")
    environment = {"PYTHONPATH": str(project)} if relative == "sitecustomize.py" else None
    assert_pytest_attack_blocked(project, environment=environment)


def test_hermetic_pytest_ignores_pythonpath_bait(tmp_path: Path) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    attack_path = tmp_path / "pythonpath"
    attack_path.mkdir()
    (attack_path / "pytest.py").write_text("raise SystemExit(0)\n", encoding="utf-8")
    assert_pytest_attack_blocked(
        project,
        environment={"PYTHONPATH": str(attack_path)},
    )


@pytest.mark.parametrize("directory", ["", "tests"])
@pytest.mark.parametrize("hook", PYTEST_CONFTEST_HOOKS)
def test_hermetic_pytest_disables_conftest_hooks(
    tmp_path: Path,
    directory: str,
    hook: str,
) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    (project / directory / "conftest.py").write_text(hook, encoding="utf-8")
    assert_pytest_attack_blocked(project)


def test_hermetic_pytest_disables_automatic_plugins(tmp_path: Path) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    python = create_autoloading_pytest_plugin(tmp_path)
    assert_pytest_attack_blocked(project, python=python)


@pytest.mark.parametrize("event_name", ["pull_request", "push"])
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
    else:
        payload = {"before": base, "after": head}

    result = run_whitespace_check(
        repo,
        tmp_path / f"{event_name}.json",
        event_name,
        payload,
    )

    assert result.returncode != 0, result.stdout + result.stderr
    assert "trailing whitespace" in result.stdout


def test_whitespace_check_uses_pull_request_merge_base(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    root = commit_files(
        repo,
        {"sample.txt": "legacy trailing whitespace \n"},
        "root",
    )
    run_git(repo, "branch", "pr-head", root)
    base = commit_files(
        repo,
        {"sample.txt": "legacy trailing whitespace\n"},
        "base fixes legacy whitespace",
    )
    run_git(repo, "checkout", "pr-head")
    head = commit_files(repo, {"clean.txt": "clean\n"}, "PR adds clean file")

    result = run_whitespace_check(
        repo,
        tmp_path / "pull-request.json",
        "pull_request",
        {
            "pull_request": {
                "base": {"sha": base},
                "head": {"sha": head},
            }
        },
    )

    assert result.returncode == 0, result.stdout + result.stderr


def test_whitespace_check_covers_entire_multi_commit_push(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    base = commit_files(repo, {"sample.txt": "clean\n"}, "base")
    commit_files(
        repo,
        {"sample.txt": "trailing whitespace \n"},
        "introduce whitespace",
    )
    head = commit_files(repo, {"other.txt": "clean\n"}, "later commit")

    result = run_whitespace_check(
        repo,
        tmp_path / "push.json",
        "push",
        {"before": base, "after": head},
    )

    assert result.returncode != 0
    assert "trailing whitespace" in result.stdout


def test_whitespace_check_passes_clean_push_and_pull_request(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    base = commit_files(repo, {"sample.txt": "clean\n"}, "base")
    head = commit_files(repo, {"sample.txt": "still clean\n"}, "candidate")
    payloads = {
        "push": {"before": base, "after": head},
        "pull_request": {
            "pull_request": {
                "base": {"sha": base},
                "head": {"sha": head},
            }
        },
    }

    for event_name, payload in payloads.items():
        result = run_whitespace_check(
            repo,
            tmp_path / f"{event_name}.json",
            event_name,
            payload,
        )
        assert result.returncode == 0, result.stdout + result.stderr


@pytest.mark.parametrize(
    ("payload_factory", "expected_message"),
    [
        (lambda _base, head: {"after": head}, "push.before must be a full"),
        (lambda base, _head: {"before": base}, "push.after must be a full"),
        (
            lambda _base, head: {"before": ZERO_OBJECT_ID, "after": head},
            "push.before must not be the zero",
        ),
        (
            lambda base, _head: {"before": base, "after": ZERO_OBJECT_ID},
            "push.after must not be the zero",
        ),
        (
            lambda _base, head: {
                "before": UNREACHABLE_OBJECT_ID,
                "after": head,
            },
            "push.before is not available",
        ),
    ],
)
def test_whitespace_check_rejects_unsafe_push_ranges(
    tmp_path: Path,
    payload_factory: Callable[[str, str], dict[str, object]],
    expected_message: str,
) -> None:
    repo = tmp_path / "repo"
    base, head = create_git_repo(repo)

    result = run_whitespace_check(
        repo,
        tmp_path / "push.json",
        "push",
        payload_factory(base, head),
    )

    assert result.returncode == 2
    assert expected_message in result.stderr


def test_whitespace_check_rejects_zero_before_on_root_push(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    head = commit_files(
        repo,
        {"sample.txt": "root trailing whitespace \n"},
        "root",
    )

    result = run_whitespace_check(
        repo,
        tmp_path / "push.json",
        "push",
        {"before": ZERO_OBJECT_ID, "after": head},
    )

    assert result.returncode == 2
    assert "push.before must not be the zero" in result.stderr


def test_whitespace_check_rejects_unavailable_pull_request_commit(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    base, _head = create_git_repo(repo)

    result = run_whitespace_check(
        repo,
        tmp_path / "pull-request.json",
        "pull_request",
        {
            "pull_request": {
                "base": {"sha": base},
                "head": {"sha": UNREACHABLE_OBJECT_ID},
            }
        },
    )

    assert result.returncode == 2
    assert "pull_request.head.sha is not available" in result.stderr


def test_whitespace_check_rejects_missing_pull_request_range(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    initialize_git_repo(repo)
    commit_files(repo, {"sample.txt": "clean\n"}, "base")

    result = run_whitespace_check(
        repo,
        tmp_path / "pull-request.json",
        "pull_request",
        {"pull_request": {}},
    )

    assert result.returncode == 2
    assert "base and head payloads are required" in result.stderr


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


def test_whitespace_check_rejects_unsupported_event_and_arguments(
    tmp_path: Path,
) -> None:
    repo = tmp_path / "repo"
    base, head = create_git_repo(repo)

    unsupported = run_whitespace_check(
        repo,
        tmp_path / "workflow-dispatch.json",
        "workflow_dispatch",
        {"before": base, "after": head},
    )
    with_arguments = run_whitespace_check(
        repo,
        tmp_path / "push.json",
        "push",
        {"before": base, "after": head},
        ["HEAD^"],
    )

    assert unsupported.returncode == 2
    assert "unsupported GitHub event" in unsupported.stderr
    assert with_arguments.returncode == 2
    assert "does not accept arguments" in with_arguments.stderr


REPOSITORY_PYTEST_RUN = f"        run: {REPOSITORY_PYTEST_COMMAND}"
REPOSITORY_PYTEST_STEP = (
    "      - name: Test repository contracts\n"
    "        env:\n"
    '          PYTEST_DISABLE_PLUGIN_AUTOLOAD: "1"\n'
    f"{REPOSITORY_PYTEST_RUN}"
)

CI_MUTATIONS = [
    (
        "remove fan-in dependency",
        "needs: [changed, storage-legacy-openers, repository-checks, "
        "fmt, web-build, clippy, test, audit]",
        "needs: [changed, storage-legacy-openers, fmt, web-build, clippy, test, audit]",
    ),
    (
        "fan-in shell suffix",
        "        run: python3 scripts/check_ci_results.py",
        "        run: python3 scripts/check_ci_results.py || true",
    ),
    (
        "fan-in multi-command block",
        "        run: python3 scripts/check_ci_results.py",
        "        run: |\n          python3 scripts/check_ci_results.py\n          echo bypass",
    ),
    (
        "fan-in shell override",
        "        run: python3 scripts/check_ci_results.py",
        "        shell: bash {0} || true\n        run: python3 scripts/check_ci_results.py",
    ),
    (
        "fan-in result binding",
        "          HARNESS_CI_RESULT_AUDIT: ${{ needs.audit.result }}",
        "          HARNESS_CI_RESULT_BOGUS: ${{ needs.audit.result }}",
    ),
    (
        "fan-in checkout path",
        "      - uses: actions/checkout@v4\n"
        "      - name: Check all jobs",
        "      - uses: actions/checkout@v4\n"
        "        with:\n"
        "          path: source\n"
        "      - name: Check all jobs",
    ),
    (
        "workflow shell default",
        "env:\n  CARGO_TERM_COLOR: always",
        "defaults:\n  run:\n    shell: bash {0} || true\n\nenv:\n  CARGO_TERM_COLOR: always",
    ),
    (
        "workflow PATH override",
        "env:\n  CARGO_TERM_COLOR: always",
        "env:\n  CARGO_TERM_COLOR: always\n  PATH: bypass",
    ),
    (
        "repository pytest environment",
        REPOSITORY_PYTEST_STEP,
        REPOSITORY_PYTEST_STEP.replace(
            "        env:\n",
            "        env:\n          PYTEST_ADDOPTS: --collect-only\n",
        ),
    ),
    (
        "repository pytest PATH",
        REPOSITORY_PYTEST_STEP,
        REPOSITORY_PYTEST_STEP.replace(
            "        env:\n",
            "        env:\n          PATH: bypass\n",
        ),
    ),
    (
        "repository pytest working directory",
        REPOSITORY_PYTEST_STEP,
        REPOSITORY_PYTEST_STEP.replace(
            "        env:\n",
            "        working-directory: bypass\n        env:\n",
        ),
    ),
    (
        "repository checkout depth",
        "          fetch-depth: 0",
        "          fetch-depth: 1",
    ),
    (
        "repository pytest conditional",
        REPOSITORY_PYTEST_STEP,
        REPOSITORY_PYTEST_STEP.replace(
            "        env:\n",
            "        if: ${{ false }}\n        env:\n",
        ),
    ),
    (
        "repository pytest command guard",
        REPOSITORY_PYTEST_RUN,
        "        run: |\n"
        "          if false; then\n"
        f"            {REPOSITORY_PYTEST_COMMAND}\n"
        "          fi",
    ),
    (
        "whitespace event override",
        "      - name: Check committed whitespace\n"
        "        run: python3 scripts/check_committed_whitespace.py",
        "      - name: Check committed whitespace\n"
        "        env:\n"
        "          GITHUB_EVENT_NAME: workflow_dispatch\n"
        "          GITHUB_EVENT_PATH: bypass-event.json\n"
        "        run: python3 scripts/check_committed_whitespace.py",
    ),
    (
        "whitespace PYTHONPATH override",
        "      - name: Check committed whitespace\n"
        "        run: python3 scripts/check_committed_whitespace.py",
        "      - name: Check committed whitespace\n"
        "        env:\n"
        "          PYTHONPATH: bypass\n"
        "        run: python3 scripts/check_committed_whitespace.py",
    ),
    (
        "whitespace continue on error",
        "      - name: Check committed whitespace\n"
        "        run: python3 scripts/check_committed_whitespace.py",
        "      - name: Check committed whitespace\n"
        "        continue-on-error: true\n"
        "        run: python3 scripts/check_committed_whitespace.py",
    ),
    (
        "whitespace command suffix",
        "        run: python3 scripts/check_committed_whitespace.py",
        "        run: python3 scripts/check_committed_whitespace.py || true",
    ),
    (
        "repository job environment",
        "  repository-checks:\n    name: Repository Checks",
        "  repository-checks:\n"
        "    name: Repository Checks\n"
        "    env:\n"
        "      PYTEST_ADDOPTS: --collect-only",
    ),
    (
        "repository job defaults",
        "  repository-checks:\n    name: Repository Checks",
        "  repository-checks:\n"
        "    name: Repository Checks\n"
        "    defaults:\n"
        "      run:\n"
        "        shell: bash {0} || true",
    ),
    (
        "fan-in working directory",
        "      - name: Check all jobs\n        env:",
        "      - name: Check all jobs\n        working-directory: bypass\n        env:",
    ),
    (
        "fan-in job PYTHONPATH",
        "  ci-result:\n    name: CI Result",
        "  ci-result:\n"
        "    name: CI Result\n"
        "    env:\n"
        "      PYTHONPATH: bypass",
    ),
    (
        "fan-in extra step environment",
        "        env:\n          HARNESS_CI_RESULT_CHANGED:",
        "        env:\n"
        "          PYTHONPATH: bypass\n"
        "          HARNESS_CI_RESULT_CHANGED:",
    ),
    (
        "changed job skipped",
        "  changed:\n    name: Detect Changes",
        "  changed:\n    name: Detect Changes\n    if: false",
    ),
    (
        "storage job skipped",
        "  storage-legacy-openers:\n    name: Storage Legacy Openers\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: changed\n"
        "    if: needs.changed.outputs.rust == 'true' || "
        "needs.changed.outputs.ci == 'true'",
        "  storage-legacy-openers:\n    name: Storage Legacy Openers\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: changed\n"
        "    if: false",
    ),
    (
        "clippy job skipped",
        "  clippy:\n    name: Clippy\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: [changed, web-build]\n"
        "    if: needs.changed.outputs.rust == 'true' || "
        "needs.changed.outputs.ci == 'true'",
        "  clippy:\n    name: Clippy\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: [changed, web-build]\n"
        "    if: false",
    ),
    (
        "test job skipped",
        "  test:\n    name: Test\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: [changed, web-build]\n"
        "    if: needs.changed.outputs.rust == 'true' || "
        "needs.changed.outputs.ci == 'true'",
        "  test:\n    name: Test\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: [changed, web-build]\n"
        "    if: false",
    ),
    (
        "audit job skipped",
        "  audit:\n    name: Security Audit\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: changed\n"
        "    if: needs.changed.outputs.rust == 'true' || "
        "needs.changed.outputs.ci == 'true'",
        "  audit:\n    name: Security Audit\n"
        "    runs-on: ubuntu-latest\n"
        "    needs: changed\n"
        "    if: false",
    ),
    (
        "changed filter coverage",
        "              - 'scripts/check_committed_whitespace.py'",
        "              - 'scripts/not-the-whitespace-check.py'",
    ),
    (
        "storage self-test",
        "      - run: python3 scripts/check_storage_legacy_openers.py --self-test",
        "      - run: echo self-test-disabled",
    ),
    (
        "repository test command",
        REPOSITORY_PYTEST_RUN,
        "        run: echo tests-disabled",
    ),
    (
        "format command",
        "      - run: cargo fmt --all -- --check",
        "      - run: echo format-disabled",
    ),
    (
        "web build command",
        "          bun run build",
        "          echo build-disabled",
    ),
    (
        "clippy command",
        "      - run: cargo clippy --workspace --all-targets -- -D warnings",
        "      - run: echo clippy-disabled",
    ),
    (
        "clippy step environment",
        "      - run: cargo clippy --workspace --all-targets -- -D warnings",
        "      - run: cargo clippy --workspace --all-targets -- -D warnings\n"
        "        env:\n"
        '          HARNESS_SKIP_WEB_BUILD: "0"',
    ),
    (
        "cargo test command",
        "      - run: cargo test ${{ steps.scope.outputs.packages }}",
        "      - run: echo tests-disabled",
    ),
    (
        "cargo test skip environment",
        "      - run: cargo test ${{ steps.scope.outputs.packages }}\n"
        "        env:\n"
        "          HARNESS_DATABASE_URL:",
        "      - run: cargo test ${{ steps.scope.outputs.packages }}\n"
        "        env:\n"
        '          HARNESS_SKIP_WEB_BUILD: "0"\n'
        "          HARNESS_DATABASE_URL:",
    ),
    (
        "server fast command",
        "        run: scripts/test-server-fast.sh",
        "        run: echo server-fast-disabled",
    ),
    (
        "server database command",
        "        run: scripts/test-server-db.sh",
        "        run: echo server-db-disabled",
    ),
    (
        "audit action",
        "      - uses: rustsec/audit-check@v2.0.0",
        "      - uses: actions/checkout@v4",
    ),
    (
        "changed output binding",
        "      workspace: ${{ steps.filter.outputs.workspace }}",
        "      workspace: 'false'",
    ),
    (
        "test timeout",
        "    timeout-minutes: 15",
        "    timeout-minutes: 1",
    ),
    (
        "postgres service",
        "        image: postgres:16",
        "        image: postgres:latest",
    ),
    (
        "web build skip environment",
        '      HARNESS_SKIP_WEB_BUILD: "1"',
        '      HARNESS_SKIP_WEB_BUILD: "0"',
    ),
    (
        "pull request trigger",
        "  pull_request:\n    branches: [main]",
        "  pull_request:\n    branches: [develop]",
    ),
]


@pytest.mark.parametrize(("label", "old", "new"), CI_MUTATIONS)
def test_ci_contract_rejects_execution_mutations(
    label: str,
    old: str,
    new: str,
) -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")
    mutated = workflow.replace(old, new)
    assert mutated != workflow, f"mutation target is missing: {label}"

    with pytest.raises(AssertionError):
        assert_ci_contract(mutated, hook)


def test_ci_contract_rejects_duplicate_keys() -> None:
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    hook = PRE_COMMIT_HOOK.read_text(encoding="utf-8")
    mutated = workflow.replace(
        "  changed:\n    name: Detect Changes",
        "  changed:\n    name: Detect Changes\n    name: Duplicate",
    )

    with pytest.raises(AssertionError, match="duplicate mapping key"):
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
