from __future__ import annotations

import os
import subprocess
import sys
import xml.etree.ElementTree as ET
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


PROTECTED_CONTRACT_PATHS = (
    ".github/workflows/repository-contract.yml",
    ".githooks/pre-commit",
    "checks/task_event_liveness.py",
    "scripts/check_ci_results.py",
    "scripts/check_committed_whitespace.py",
    "scripts/verify_repository_contract.py",
    "tests/ci_contract_support.py",
    "tests/test_ci_contract.py",
    "tests/test_ci_trust_boundary.py",
    "tests/test_task_event_liveness.py",
)


def assert_contract_implementation_matches_trusted_base(
    trusted_root: Path = TRUSTED_ROOT,
    protected_paths: tuple[str, ...] = PROTECTED_CONTRACT_PATHS,
) -> None:
    changed: list[str] = []
    for relative in protected_paths:
        candidate = contract_candidate_file(relative)
        trusted = trusted_root / relative
        if candidate.read_bytes() != trusted.read_bytes():
            changed.append(relative)
    assert not changed or os.environ.get(
        "HARNESS_CONTRACT_TRUST_ROTATION"
    ) == "true", (
        "protected repository-contract files changed without the "
        f"repository-contract-update label: {changed}"
    )


def test_candidate_contract_implementation_matches_trusted_base() -> None:
    assert_contract_implementation_matches_trusted_base()


def test_contract_implementation_change_requires_trust_rotation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    trusted = tmp_path / "trusted"
    candidate = tmp_path / "candidate"
    trusted.mkdir()
    candidate.mkdir()
    (trusted / "contract").write_text("trusted", encoding="utf-8")
    (candidate / "contract").write_text("candidate", encoding="utf-8")
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", candidate)
    monkeypatch.delenv("HARNESS_CONTRACT_TRUST_ROTATION", raising=False)
    with pytest.raises(AssertionError, match="without the.*label"):
        assert_contract_implementation_matches_trusted_base(
            trusted, ("contract",)
        )


def test_contract_implementation_change_accepts_trust_rotation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    trusted = tmp_path / "trusted"
    candidate = tmp_path / "candidate"
    trusted.mkdir()
    candidate.mkdir()
    (trusted / "contract").write_text("trusted", encoding="utf-8")
    (candidate / "contract").write_text("candidate", encoding="utf-8")
    monkeypatch.setattr(contract_support, "CANDIDATE_ROOT", candidate)
    monkeypatch.setenv("HARNESS_CONTRACT_TRUST_ROTATION", "true")
    assert_contract_implementation_matches_trusted_base(trusted, ("contract",))


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


REQUIRED_REPORT_CASES = (
    ("test_ci_contract", "test_scoped_ci_pipeline_contract"),
    (
        "test_ci_trust_boundary",
        "test_candidate_contract_implementation_matches_trusted_base",
    ),
    (
        "test_ci_trust_boundary",
        "test_isolated_pip_ignores_repository_package",
    ),
    (
        "test_task_event_liveness",
        "test_liveness_audit_cli_returns_nonzero_for_limbo",
    ),
)


def run_repository_contract_verifier(
    tmp_path: Path,
    cases: list[tuple[str, str, str | None]],
    collected_count: int,
) -> subprocess.CompletedProcess[str]:
    report = tmp_path / "report.xml"
    root = ET.Element("testsuites")
    suite = ET.SubElement(root, "testsuite")
    for classname, name, outcome in cases:
        case = ET.SubElement(
            suite, "testcase", {"classname": classname, "name": name}
        )
        if outcome is not None:
            ET.SubElement(case, outcome)
    ET.ElementTree(root).write(report, encoding="unicode")
    collection = tmp_path / "collection.txt"
    collection.write_text(
        f"{collected_count} tests collected in 0.01s\n", encoding="utf-8"
    )
    environment = os.environ.copy()
    environment.update(
        {
            "HARNESS_CONTRACT_REPORT": str(report),
            "HARNESS_CONTRACT_COLLECTION": str(collection),
        }
    )
    return subprocess.run(
        [
            sys.executable,
            "-I",
            str(TRUSTED_ROOT / "scripts/verify_repository_contract.py"),
        ],
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )


def test_repository_contract_verifier_uses_dynamic_collection_count(
    tmp_path: Path,
) -> None:
    cases = [(classname, name, None) for classname, name in REQUIRED_REPORT_CASES]
    cases.append(("test_future_contract", "test_future_behavior", None))
    result = run_repository_contract_verifier(tmp_path, cases, len(cases))
    assert result.returncode == 0, result.stderr
    assert "verified 5 trusted repository-contract nodes" in result.stdout


def test_repository_contract_verifier_rejects_missing_execution(
    tmp_path: Path,
) -> None:
    cases = [(classname, name, None) for classname, name in REQUIRED_REPORT_CASES]
    result = run_repository_contract_verifier(tmp_path, cases, len(cases) + 1)
    assert result.returncode != 0
    assert "expected collected count 5" in result.stderr


def test_repository_contract_verifier_rejects_duplicate_nodes(
    tmp_path: Path,
) -> None:
    cases = [(classname, name, None) for classname, name in REQUIRED_REPORT_CASES]
    cases.append(cases[0])
    result = run_repository_contract_verifier(tmp_path, cases, len(cases))
    assert result.returncode != 0
    assert "duplicate nodes" in result.stderr


def test_repository_contract_verifier_rejects_nonpassing_node(
    tmp_path: Path,
) -> None:
    cases = [(classname, name, None) for classname, name in REQUIRED_REPORT_CASES]
    classname, name, _ = cases[0]
    cases[0] = (classname, name, "skipped")
    result = run_repository_contract_verifier(tmp_path, cases, len(cases))
    assert result.returncode != 0
    assert "did not pass" in result.stderr
