from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import ci_contract_support as contract_support
import pytest
from ci_contract_support import (
    PYTEST_CONFIG_BAITS,
    PYTEST_CONFTEST_HOOKS,
    PYTEST_ROOT_BAITS,
    TRUSTED_ROOT,
    assert_pytest_attack_blocked,
    contract_candidate_file,
    create_autoloading_pytest_plugin,
    create_pytest_canary,
)


def test_candidate_contract_file_accepts_bounded_regular_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", tmp_path)
    candidate = tmp_path / "contract"
    candidate.write_text("content", encoding="utf-8")
    assert contract_support.contract_candidate_file("contract") == candidate.resolve()


def test_candidate_contract_file_rejects_symlink(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", tmp_path)
    target = tmp_path / "target"
    target.write_text("content", encoding="utf-8")
    (tmp_path / "contract").symlink_to(target)
    with pytest.raises(AssertionError, match="not a regular file"):
        contract_support.contract_candidate_file("contract")


def test_candidate_contract_file_rejects_escape(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "candidate"
    root.mkdir()
    (tmp_path / "outside").write_text("content", encoding="utf-8")
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", root)
    with pytest.raises(AssertionError, match="escapes checkout"):
        contract_support.contract_candidate_file("../outside")


def test_candidate_contract_file_rejects_oversize(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", tmp_path)
    (tmp_path / "contract").write_bytes(b"xx")
    with pytest.raises(AssertionError, match="too large"):
        contract_support.contract_candidate_file("contract", max_bytes=1)


CODEOWNER_CONTRACT_PATHS = (
    "/.github/",
    "/.githooks/",
    "/.bun-version",
    "/AGENTS.md",
    "/CLAUDE.md",
    "/checks/",
    "/scripts/",
    "/tests/",
)


def test_repository_contract_paths_require_owner_review() -> None:
    entries: dict[str, list[str]] = {}
    codeowners = TRUSTED_ROOT / ".github/CODEOWNERS"
    for raw_line in codeowners.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        path, *owners = line.split()
        entries[path] = owners
    for path in CODEOWNER_CONTRACT_PATHS:
        assert entries.get(path) == ["@majiayu000"], path


@pytest.mark.parametrize(("relative", "content"), PYTEST_CONFIG_BAITS)
def test_hermetic_pytest_ignores_repository_config(
    tmp_path: Path,
    relative: str,
    content: str,
) -> None:
    project = tmp_path / "project"
    create_pytest_canary(project)
    (project / relative).write_text(content, encoding="utf-8")
    assert_pytest_attack_blocked(project)


@pytest.mark.parametrize(("relative", "content"), PYTEST_ROOT_BAITS)
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
    assert_pytest_attack_blocked(project, environment={"PYTHONPATH": str(attack_path)})


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


def test_isolated_pip_ignores_repository_package(tmp_path: Path) -> None:
    fake_pip = tmp_path / "pip"
    fake_pip.mkdir()
    (fake_pip / "__init__.py").write_text("", encoding="utf-8")
    (fake_pip / "__main__.py").write_text("print('HIJACKED')\n", encoding="utf-8")
    attack_environment = os.environ.copy()
    attack_environment["PYTHONPATH"] = str(tmp_path)
    bypassed = subprocess.run(
        [sys.executable, "-m", "pip", "--version"],
        cwd=tmp_path,
        env=attack_environment,
        check=False,
        capture_output=True,
        text=True,
    )
    assert bypassed.returncode == 0
    assert "HIJACKED" in bypassed.stdout
    result = subprocess.run(
        [sys.executable, "-I", "-m", "pip", "--version"],
        cwd=tmp_path,
        env=attack_environment,
        check=False,
        capture_output=True,
        text=True,
    )
    output = result.stdout + result.stderr
    assert "HIJACKED" not in output
    assert result.returncode == 0 or "No module named pip" in output
