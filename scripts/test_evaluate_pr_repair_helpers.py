#!/usr/bin/env python3
"""Tests for PR repair eval helper scripts."""

from __future__ import annotations

import argparse
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


SCRIPT_DIR = Path(__file__).parent
PREFLIGHT = load_module(
    "evaluate_pr_repair_preflight",
    SCRIPT_DIR / "evaluate_pr_repair_preflight.py",
)
REPORT = load_module(
    "evaluate_pr_repair_render_report",
    SCRIPT_DIR / "evaluate_pr_repair_render_report.py",
)


class PreflightTests(unittest.TestCase):
    def test_find_registered_project_matches_canonical_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()
            projects = [{"root": str(root.resolve()), "active": True}]

            self.assertEqual(
                PREFLIGHT.find_registered_project(str(root), projects),
                str(root.resolve()),
            )

    def test_find_registered_project_rejects_unregistered_root(self) -> None:
        with self.assertRaisesRegex(ValueError, "not registered"):
            PREFLIGHT.find_registered_project("/tmp/missing", [{"root": "/tmp/other"}])

    def test_find_registered_project_rejects_existing_unregistered_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            root.mkdir()

            with self.assertRaisesRegex(ValueError, "not registered"):
                PREFLIGHT.find_registered_project(str(root), [{"root": "/tmp/other"}])

    def test_find_registered_project_matches_id_and_name(self) -> None:
        projects = [
            {
                "id": "/tmp/project-root",
                "name": "harness",
                "root": "/tmp/project-root",
                "active": True,
            }
        ]

        self.assertEqual(
            PREFLIGHT.find_registered_project("harness", projects),
            "/tmp/project-root",
        )
        self.assertEqual(
            PREFLIGHT.find_registered_project("/tmp/project-root", projects),
            "/tmp/project-root",
        )

    def test_find_registered_project_requires_usable_root_for_name_match(self) -> None:
        projects = [{"id": "project-id", "name": "harness", "root": 42, "active": True}]

        with self.assertRaisesRegex(ValueError, "no usable root"):
            PREFLIGHT.find_registered_project("harness", projects)

    def test_write_preflight_failure_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            submission = Path(tmp) / "submission.json"
            task_detail = Path(tmp) / "task_detail.json"

            PREFLIGHT.write_preflight_failure(submission, task_detail, "not registered")

            self.assertEqual(json.loads(submission.read_text())["status"], "failed")
            self.assertEqual(json.loads(task_detail.read_text())["preflight"], "server_project_registry")


class RenderReportTests(unittest.TestCase):
    def test_render_report_uses_quality_snapshot_grade(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = root / "baseline.json"
            final = root / "final.json"
            submission = root / "submission.json"
            task_detail = root / "task_detail.json"
            quality = root / "quality.json"

            pr = {
                "headRefOid": "a",
                "mergeStateStatus": "CLEAN",
                "statusCheckRollup": {"state": "SUCCESS"},
                "reviewThreads": {"nodes": []},
                "files": {"nodes": []},
            }
            baseline.write_text(json.dumps({**pr, "number": 1}), encoding="utf-8")
            final.write_text(json.dumps({**pr, "number": 1}), encoding="utf-8")
            submission.write_text(json.dumps({"status": "failed"}), encoding="utf-8")
            task_detail.write_text(json.dumps({"status": "failed"}), encoding="utf-8")
            quality.write_text(
                json.dumps(
                    {
                        "scenario": "pr_repair",
                        "final_grade": "D",
                        "final_score": 54,
                        "grade_cap": "C",
                        "blocker_summary": ["runtime artifact is missing"],
                        "hard_gates": [
                            {
                                "name": "runtime_artifact_completeness",
                                "status": "fail",
                                "grade_cap": "B",
                                "message": "runtime artifact is missing",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            args = argparse.Namespace(
                baseline=baseline,
                final=final,
                submission=submission,
                task_detail=task_detail,
                quality_snapshot=quality,
                run_id="run",
                repo="owner/repo",
                pr="1",
                server_url="http://127.0.0.1:9800",
                timed_out=False,
                wait_secs="1",
                max_rounds="1",
                max_turns="1",
                max_budget_usd="",
                timeout_secs="5",
            )

            rendered = REPORT.render_report(args)

            self.assertIn("- Scenario: `pr_repair`", rendered)
            self.assertIn("- Grade: `D`", rendered)
            self.assertIn("- Score: `54`", rendered)
            self.assertIn("- runtime artifact is missing", rendered)
            self.assertIn("`runtime_artifact_completeness`", rendered)

    def test_render_report_tolerates_malformed_optional_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = root / "baseline.json"
            final = root / "final.json"
            submission = root / "submission.json"
            task_detail = root / "task_detail.json"
            quality = root / "quality.json"

            baseline.write_text(
                json.dumps(
                    {
                        "headRefOid": "a",
                        "mergeStateStatus": "DIRTY",
                        "statusCheckRollup": "unexpected",
                        "reviewThreads": "unexpected",
                        "files": {"nodes": ["unexpected", {"path": "src/lib.rs"}]},
                    }
                ),
                encoding="utf-8",
            )
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
            submission.write_text(json.dumps({"status": "running"}), encoding="utf-8")
            task_detail.write_text(json.dumps({"workflow": "unexpected"}), encoding="utf-8")
            quality.write_text(
                json.dumps({"scenario": "pr_repair", "final_grade": "C"}),
                encoding="utf-8",
            )
            args = argparse.Namespace(
                baseline=baseline,
                final=final,
                submission=submission,
                task_detail=task_detail,
                quality_snapshot=quality,
                run_id="run",
                repo="owner/repo",
                pr="1",
                server_url="http://127.0.0.1:9800",
                timed_out=False,
                wait_secs="1",
                max_rounds="1",
                max_turns="1",
                max_budget_usd="",
                timeout_secs="5",
            )

            rendered = REPORT.render_report(args)

            self.assertIn("- Scenario: `pr_repair`", rendered)
            self.assertIn("| `statusCheckRollup.state` | `UNKNOWN` | `SUCCESS` |", rendered)


if __name__ == "__main__":
    unittest.main()
