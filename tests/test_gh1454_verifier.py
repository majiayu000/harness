from __future__ import annotations

import hashlib
import json
import subprocess
import tarfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "evals" / "verifiers" / "gh1454_ci_contract_v1.json"
PINNED_BASE = "9c0099ad458e82fd377fd20a8e288a46722762ef"
ACCEPTED_GOLD = "e8de36b98b0afc1c8213486b3ff89ec7af5e4d2d"
VERIFIED_PATHS = (".github/workflows/ci.yml", ".githooks/pre-commit")


def _extract_revision(revision: str, destination: Path) -> None:
    archive = destination / "candidate.tar"
    with archive.open("wb") as output:
        subprocess.run(
            ["git", "archive", "--format=tar", revision, *VERIFIED_PATHS],
            cwd=ROOT,
            check=True,
            stdout=output,
        )
    with tarfile.open(archive) as bundle:
        bundle.extractall(destination, filter="data")
    archive.unlink()


def _run_verifier(workspace: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "-p",
            "harness-cli",
            "--bin",
            "harness",
            "--",
            "eval",
            "verify-trusted",
            "gh1454_ci_contract_v1",
            "--workspace",
            str(workspace),
            "--verifier-sha256",
            hashlib.sha256(CONTRACT.read_bytes()).hexdigest(),
        ],
        cwd=ROOT,
        check=False,
        text=True,
        capture_output=True,
    )


def test_gh1454_pinned_base_fails_and_accepted_gold_passes(tmp_path: Path) -> None:
    base = tmp_path / "base"
    gold = tmp_path / "gold"
    base.mkdir()
    gold.mkdir()
    _extract_revision(PINNED_BASE, base)
    _extract_revision(ACCEPTED_GOLD, gold)

    base_result = _run_verifier(base)
    gold_result = _run_verifier(gold)

    assert base_result.returncode != 0, base_result.stdout + base_result.stderr
    assert gold_result.returncode == 0, gold_result.stdout + gold_result.stderr
    gold_evidence = json.loads(gold_result.stdout)
    assert gold_evidence == {
        "errors": [],
        "passed": True,
        "verifier_id": "gh1454_ci_contract_v1",
        "verifier_sha256": hashlib.sha256(CONTRACT.read_bytes()).hexdigest(),
    }
