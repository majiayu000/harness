#!/usr/bin/env python3
"""Tests for PR repair eval artifact helpers."""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).with_name("evaluate_pr_repair_artifacts.py")
sys.path.insert(0, str(SCRIPT_PATH.parent))
SPEC = importlib.util.spec_from_file_location("evaluate_pr_repair_artifacts", SCRIPT_PATH)
assert SPEC is not None
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)

from evaluate_pr_repair_submit import write_json as write_json_file


def pr_snapshot(head: str = "abc123") -> dict[str, object]:
    return {
        "number": 7,
        "headRefOid": head,
        "mergeStateStatus": "CLEAN",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {"nodes": []},
        "files": {"nodes": []},
    }


class ArtifactHelperTests(unittest.TestCase):
    def test_project_registry_preflight_requires_registered_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            registered = tmp_path / "registered"
            projects_json = tmp_path / "projects.json"
            write_json_file(
                projects_json,
                [{"root": str(registered), "id": str(registered), "active": True}],
            )

            self.assertEqual(
                MODULE.project_registry_preflight(
                    argparse.Namespace(
                        project_root=str(registered),
                        projects_json=projects_json,
                    )
                ),
                0,
            )
            self.assertEqual(
                MODULE.project_registry_preflight(
                    argparse.Namespace(
                        project_root=str(tmp_path / "unregistered"),
                        projects_json=projects_json,
                    )
                ),
                1,
            )

    def test_preflight_failure_artifacts_are_structured(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            submission = tmp_path / "submission.json"
            task_detail = tmp_path / "task_detail_final.json"

            MODULE.write_preflight_failure(
                argparse.Namespace(
                    submission=submission,
                    task_detail=task_detail,
                    project_root="/tmp/project",
                    server_url="http://127.0.0.1:9800",
                    error="project registry preflight failed",
                )
            )

            self.assertEqual(json.loads(submission.read_text())["http_status"], "preflight")
            self.assertEqual(
                json.loads(task_detail.read_text())["stage"],
                "project_registry_preflight",
            )

    def test_final_report_uses_quality_snapshot_grade_and_blockers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            final = tmp_path / "final.json"
            quality = tmp_path / "quality_snapshot.json"
            submission = tmp_path / "submission.json"
            task_detail = tmp_path / "task_detail_final.json"
            report = tmp_path / "summary.md"

            write_json_file(baseline, pr_snapshot())
            write_json_file(final, pr_snapshot())
            write_json_file(
                quality,
                {
                    "final_grade": "C",
                    "final_score": 70,
                    "grade_cap": "C",
                    "blocker_summary": ["quality blocker"],
                },
            )
            write_json_file(
                submission,
                {
                    "status": "failed",
                    "error": "project registry preflight failed",
                    "eval_submission_mode": "prompt_task",
                    "http_status": "preflight",
                },
            )
            write_json_file(task_detail, {"status": "failed"})

            MODULE.write_final_report(
                argparse.Namespace(
                    baseline=baseline,
                    final=final,
                    submission=submission,
                    task_detail=task_detail,
                    quality=quality,
                    output=report,
                    run_id="run-1",
                    repo="owner/repo",
                    pr="7",
                    server_url="http://127.0.0.1:9800",
                    timed_out="0",
                    wait_secs="10",
                    max_rounds="2",
                    max_turns="6",
                    max_budget_usd="",
                    timeout_secs="7200",
                )
            )

            text = report.read_text(encoding="utf-8")
            self.assertIn("- Grade: `C`", text)
            self.assertIn("- Score: `70`", text)
            self.assertIn("- Grade cap: `C`", text)
            self.assertIn("- Timed out: `false`", text)
            self.assertIn("- quality blocker", text)

    def test_final_report_tolerates_non_object_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            baseline = tmp_path / "baseline.json"
            final = tmp_path / "final.json"
            quality = tmp_path / "quality_snapshot.json"
            submission = tmp_path / "submission.json"
            task_detail = tmp_path / "task_detail_final.json"
            report = tmp_path / "summary.md"

            baseline.write_text(json.dumps(None), encoding="utf-8")
            final.write_text(
                json.dumps(
                    {
                        "headRefOid": "b",
                        "mergeStateStatus": "CLEAN",
                        "statusCheckRollup": {"state": "SUCCESS"},
                        "reviewThreads": {"nodes": ["unexpected"]},
                        "files": "unexpected",
                    }
                ),
                encoding="utf-8",
            )
            quality.write_text(json.dumps(["unexpected"]), encoding="utf-8")
            submission.write_text(json.dumps(["unexpected"]), encoding="utf-8")
            task_detail.write_text(json.dumps({"workflow": "unexpected"}), encoding="utf-8")

            MODULE.write_final_report(
                argparse.Namespace(
                    baseline=baseline,
                    final=final,
                    submission=submission,
                    task_detail=task_detail,
                    quality=quality,
                    output=report,
                    run_id="run-1",
                    repo="owner/repo",
                    pr="7",
                    server_url="http://127.0.0.1:9800",
                    timed_out="0",
                    wait_secs="10",
                    max_rounds="2",
                    max_turns="6",
                    max_budget_usd="",
                    timeout_secs="7200",
                )
            )

            text = report.read_text(encoding="utf-8")
            self.assertIn("| `statusCheckRollup.state` | `UNKNOWN` | `SUCCESS` |", text)
            self.assertIn("| changed-file evidence | `missing` |", text)
            self.assertIn("- None", text)


if __name__ == "__main__":
    unittest.main()
