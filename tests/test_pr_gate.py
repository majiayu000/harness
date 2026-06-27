from __future__ import annotations

import json
import shutil
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


def copy_workflow_pack(target: Path) -> None:
    for name in ["workflow.yaml", "states.yaml", "labels.yaml"]:
        shutil.copy(ROOT / name, target / name)


def test_pr_gate_allows_clean_authorized_merge() -> None:
    result = evaluate_pr_gate(clean_evidence())

    assert result["decision"] == "allowed"
    assert result["missing"] == []
    assert result["reasons"] == []


def test_pr_gate_rejects_boolean_ids() -> None:
    evidence = clean_evidence()
    evidence["pr"] = True
    evidence["linked_issue"] = True

    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "blocked"
    assert "pr" in result["missing"]
    assert "linked_issue" in result["missing"]


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


def test_pr_gate_accepts_skipped_and_neutral_checks() -> None:
    evidence = clean_evidence()
    evidence["checks"] = [
        {"name": "docs-only", "status": "COMPLETED", "conclusion": "SKIPPED"},
        {"name": "advisory", "status": "COMPLETED", "conclusion": "NEUTRAL"},
    ]

    result = evaluate_pr_gate(evidence)

    assert result["decision"] == "allowed"
    assert result["reasons"] == []


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


def test_pr_gate_blocks_unresolved_outdated_thread() -> None:
    evidence = clean_evidence()
    evidence["review_threads"] = [
        {
            "url": "https://example.invalid/outdated-thread",
            "is_resolved": False,
            "is_outdated": True,
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


def test_route_gate_allows_first_pr_review_without_verification_artifact() -> None:
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
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert "artifacts/review/pr-123.json" in payload["required_artifacts"]
    assert "artifacts/verification/pr-123.json" not in payload["required_artifacts"]
    assert "verification" not in payload["missing"]


def test_route_gate_returns_configured_forbidden_agent_actions() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "write_spec",
            "--issue",
            "5",
            "--state",
            "triaged",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert {
        "close_disputed_issue",
        "final_approval",
        "force_push",
        "merge",
        "permission_change",
        "public_security_disclosure",
    } <= set(payload["blocked_actions"])


def test_route_gate_infers_agent_review_state_from_review_label() -> None:
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
            "--label",
            "agent_reviewed",
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert payload["current_state"] == "agent_review"
    assert "current_state" not in payload["missing"]


def test_route_gate_rejects_conflicting_cli_and_evidence_ids(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": 10, "pr": 124}),
        encoding="utf-8",
    )

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

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert any("conflicting linked_issue" in reason for reason in payload["reasons"])
    assert any("conflicting pr" in reason for reason in payload["reasons"])


def test_route_gate_blocked_result_includes_configured_forbidden_actions() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "write_spec",
            "--issue",
            "5",
            "--state",
            "security_private",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert {
        "close_disputed_issue",
        "final_approval",
        "force_push",
        "merge",
        "permission_change",
        "public_security_disclosure",
        "write_spec",
    } <= set(payload["blocked_actions"])


def test_route_gate_blocks_terminal_states_from_config() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "write_spec",
            "--issue",
            "5",
            "--state",
            "release_note_drafted",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert payload["reasons"] == ["state release_note_drafted is terminal or maintainer-reserved"]
    assert "write_spec" in payload["blocked_actions"]


def test_route_gate_accepts_configured_label_alias_for_state(tmp_path: Path) -> None:
    copy_workflow_pack(tmp_path)
    labels_path = tmp_path / "labels.yaml"
    labels_path.write_text(
        labels_path.read_text(encoding="utf-8").replace(
            "    - ready_to_spec\n", "    - ready-to-spec\n", 1
        ),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            str(tmp_path),
            "--route",
            "write_spec",
            "--issue",
            "9",
            "--label",
            "ready-to-spec",
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert payload["current_state"] == "ready_to_spec"


def test_route_gate_blocks_unknown_required_artifact(tmp_path: Path) -> None:
    copy_workflow_pack(tmp_path)
    workflow_path = tmp_path / "workflow.yaml"
    workflow_path.write_text(
        workflow_path.read_text(encoding="utf-8").replace(
            "        - linked_issue\n        - product_spec\n        - tech_spec\n",
            "        - linked_issue\n        - product_specs\n        - tech_spec\n",
            1,
        ),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            str(tmp_path),
            "--route",
            "implement",
            "--issue",
            "9",
            "--state",
            "ready_to_implement",
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert (
        "workflow.yaml: action implement references unknown artifact product_specs"
        in payload["reasons"]
    )


def test_route_gate_renders_fix_ci_verification_artifact_path() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "fix_ci",
            "--pr",
            "123",
            "--state",
            "human_review",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert "artifacts/verification/pr-123.json" in payload["required_artifacts"]
    assert "artifacts/verification/pr-{pr_number}.json" not in payload["required_artifacts"]


def test_route_gate_uses_evidence_linked_issue_for_pr_review(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": 9, "verification": "cargo test"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--pr",
            "123",
            "--state",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert payload["issue"] == 9
    assert "linked_issue" not in payload["missing"]


def test_route_gate_uses_evidence_pr_for_pr_review(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": 9, "pr": 123, "verification": "cargo test"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--state",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert payload["issue"] == 9
    assert payload["pr"] == 123
    assert "linked_pr" not in payload["missing"]
    assert "artifacts/review/pr-123.json" in payload["required_artifacts"]
    assert "force_push" in payload["blocked_actions"]


def test_route_gate_rejects_boolean_evidence_ids(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": True, "pr": True, "verification": "cargo test"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--state",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert "linked_issue" in payload["missing"]
    assert "linked_pr" in payload["missing"]


def test_route_gate_allows_null_evidence_labels(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": 9, "pr": 123, "labels": None, "verification": "cargo test"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--state",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"


def test_route_gate_blocks_non_list_evidence_labels(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"labels": "ready_to_implement"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "implement",
            "--issue",
            "5",
            "--evidence",
            str(evidence_path),
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert payload["reasons"] == ["evidence labels must be a list when provided"]


def test_route_gate_ignores_github_pr_state_when_label_supplies_specrail_state(tmp_path: Path) -> None:
    evidence_path = tmp_path / "evidence.json"
    evidence_path.write_text(
        json.dumps({"linked_issue": 9, "pr": 123, "state": "OPEN", "verification": "cargo test"}),
        encoding="utf-8",
    )

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--label",
            "impl_pr_open",
            "--evidence",
            str(evidence_path),
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["decision"] == "allowed"
    assert payload["current_state"] == "impl_pr_open"


def test_route_gate_blocks_requested_route_when_required_artifacts_are_missing() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "implement",
            "--issue",
            "999",
            "--state",
            "ready_to_implement",
            "--mode",
            "required",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["decision"] == "blocked"
    assert "implement" in payload["blocked_actions"]
    assert "implement" not in payload["allowed_actions"]


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


def test_route_gate_rejects_off_template_artifact_paths() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "implement",
            "--issue",
            "5",
            "--state",
            "ready_to_implement",
            "--artifact",
            "product_spec=/etc/passwd",
            "--artifact",
            "tech_spec=/etc/passwd",
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
    assert "product_spec:/etc/passwd" in payload["missing"]
    assert "tech_spec:/etc/passwd" in payload["missing"]


def test_route_gate_rejects_non_positive_issue_and_pr() -> None:
    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            ".",
            "--route",
            "review_pr",
            "--issue",
            "-1",
            "--pr",
            "-1",
            "--state",
            "impl_pr_open",
            "--json",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 2
    assert "positive integer" in result.stderr
