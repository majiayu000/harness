from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKS = ROOT / "checks"
sys.path.insert(0, str(CHECKS))

from pr_gate import evaluate_pr_gate  # noqa: E402


def clean_evidence() -> dict[str, object]:
    return {
        "pr": 718,
        "state": "OPEN",
        "is_draft": False,
        "head_sha": "e36d97517d8d0b27faca1abe5e5c63f9f88684d9",
        "merge_state": "CLEAN",
        "linked_issue": 716,
        "checks": [
            {
                "name": "Compile-check all features",
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
            }
        ],
        "reviews": [{"author": "gemini-code-assist", "state": "COMMENTED"}],
        "review_threads": [
            {
                "url": "https://github.com/majiayu000/litellm-rs/pull/718#discussion_r3473213282",
                "is_resolved": True,
                "is_outdated": False,
            }
        ],
        "human_authorization": {
            "actor": "maintainer",
            "source": "chat",
            "summary": "merge approved",
        },
    }


def test_pr_gate_allows_clean_authorized_merge() -> None:
    result = evaluate_pr_gate(clean_evidence())

    assert result["decision"] == "allowed"
    assert result["missing"] == []
    assert result["reasons"] == []


def test_pr_gate_needs_human_without_authorization() -> None:
    evidence = clean_evidence()
    evidence.pop("human_authorization")

    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "needs_human"
    assert "human_authorization" in result["missing"]
    assert result["blocked_actions"] == ["merge"]


def test_pr_gate_blocks_pending_ci() -> None:
    evidence = clean_evidence()
    evidence["checks"] = [
        {
            "name": "workflow-check",
            "status": "IN_PROGRESS",
            "conclusion": "",
        }
    ]

    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "blocked"
    assert any("workflow-check is not completed" in reason for reason in result["reasons"])


def test_pr_gate_blocks_unresolved_thread() -> None:
    evidence = clean_evidence()
    evidence["review_threads"] = [
        {
            "url": "https://example.invalid/thread",
            "is_resolved": False,
            "is_outdated": False,
        }
    ]

    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "blocked"
    assert any("unresolved review threads" in reason for reason in result["reasons"])


def test_pr_gate_cli_json_contract(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(json.dumps(clean_evidence()), encoding="utf-8")

    result = subprocess.run(
        [
            sys.executable,
            "checks/pr_gate.py",
            "--repo",
            ".",
            "--evidence",
            str(evidence_path),
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert {
        "decision",
        "pr",
        "linked_issue",
        "head_sha",
        "reasons",
        "satisfied",
        "missing",
        "blocked_actions",
        "verification_commands",
    } <= set(payload)


def test_route_gate_renders_pr_artifact_path(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(json.dumps({"verification": "cargo test"}), encoding="utf-8")

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--issue",
            "9",
            "--pr",
            "123",
            "--state",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert "artifacts/review/pr-123.json" in payload["required_artifacts"]
    assert "artifacts/review/pr-{pr_number}.json" not in payload["required_artifacts"]


def test_route_gate_requires_issue_for_triage_result() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "triage_issue",
            "--state",
            "new_issue",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "warn"
    assert "linked_issue" in payload["missing"]
    assert "artifacts/triage/issue-{issue_number}.json" not in payload["required_artifacts"]
