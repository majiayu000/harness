from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKS = ROOT / "checks"
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(CHECKS))

from evaluate import evaluate  # noqa: E402


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def copy_workflow_pack(target: Path) -> None:
    for name in ["workflow.yaml", "states.yaml", "labels.yaml"]:
        shutil.copy(ROOT / name, target / name)


def require_review_pr_verification(workflow_path: Path) -> None:
    workflow_path.write_text(
        workflow_path.read_text(encoding="utf-8").replace(
            "\n".join(
                [
                    "    review_pr:",
                    "      allowed_from:",
                    "        - impl_pr_open",
                    "        - agent_review",
                    "        - human_review",
                    "      required_artifacts:",
                    "        - linked_issue",
                    "        - linked_pr",
                    "      creates_artifacts:",
                    "        - agent_review",
                ]
            ),
            "\n".join(
                [
                    "    review_pr:",
                    "      allowed_from:",
                    "        - impl_pr_open",
                    "        - agent_review",
                    "        - human_review",
                    "      required_artifacts:",
                    "        - linked_issue",
                    "        - linked_pr",
                    "        - verification",
                    "      creates_artifacts:",
                    "        - agent_review",
                ]
            ),
        ),
        encoding="utf-8",
    )


def test_evaluate_rejects_spec_dir_outside_configured_layout(tmp_path: Path) -> None:
    copy_workflow_pack(tmp_path)
    write_text(tmp_path / "specs" / "GH13" / "product.md", "GitHub issue: `#13`\n")
    write_text(tmp_path / "specs" / "GH13" / "tech.md", "GitHub issue: `#13`\n")

    result = evaluate(tmp_path, tmp_path / "bogus" / "GH13")

    assert result["status"] == "fail"
    assert "bogus/GH13: does not match configured spec packet layout" in result["errors"]


def test_route_gate_blocks_missing_required_verification_artifact(tmp_path: Path) -> None:
    copy_workflow_pack(tmp_path)
    require_review_pr_verification(tmp_path / "workflow.yaml")

    result = subprocess.run(
        [
            sys.executable,
            "checks/route_gate.py",
            "--repo",
            str(tmp_path),
            "--route",
            "review_pr",
            "--issue",
            "9",
            "--pr",
            "123",
            "--state",
            "impl_pr_open",
            "--artifact",
            "verification=does/not/exist",
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
    assert "verification:does/not/exist:expected:artifacts/verification/pr-123.json" in payload["missing"]
