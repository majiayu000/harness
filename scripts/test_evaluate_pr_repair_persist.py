#!/usr/bin/env python3
"""Tests for persisting PR repair eval results through the Harness eval API."""

from __future__ import annotations

import argparse
import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).with_name("evaluate_pr_repair_persist.py")
sys.path.insert(0, str(SCRIPT_PATH.parent))
SPEC = importlib.util.spec_from_file_location("evaluate_pr_repair_persist", SCRIPT_PATH)
assert SPEC is not None
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class EvalApiResponseStub:
    def __init__(self, status: int, body: dict[str, object]) -> None:
        self.status = status
        self._body = json.dumps(body).encode("utf-8")

    def read(self) -> bytes:
        return self._body

    def __enter__(self) -> "EvalApiResponseStub":
        return self

    def __exit__(self, *args: object) -> bool:
        return False


def eval_input() -> dict[str, object]:
    return {
        "scenario": "pr_repair",
        "run_mode": "live_run",
        "target": {
            "kind": "pull_request",
            "repo": "owner/repo",
            "pr_number": 7,
            "base_ref": "main",
            "head_ref": "fix/review",
        },
    }


class PersistHelperTests(unittest.TestCase):
    def test_ensure_eval_run_creates_run_from_canonical_input(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            input_path = tmp_path / "pr_repair_eval_input.json"
            output_path = tmp_path / "eval_run.json"
            input_path.write_text(json.dumps(eval_input()), encoding="utf-8")
            calls: list[dict[str, object]] = []

            def eval_run_urlopen_stub(request: object, timeout: int) -> EvalApiResponseStub:
                calls.append(
                    {
                        "url": request.full_url,
                        "body": json.loads(request.data.decode("utf-8")),
                        "timeout": timeout,
                    }
                )
                return EvalApiResponseStub(201, {"run": {"id": "run-1"}})

            with mock.patch.object(
                MODULE.urllib.request,
                "urlopen",
                side_effect=eval_run_urlopen_stub,
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    status = MODULE.ensure_eval_run(
                        argparse.Namespace(
                            server_url="http://127.0.0.1:9800",
                            input=input_path,
                            source_task_id="task-1",
                            output=output_path,
                        )
                    )

            self.assertEqual(status, 0)
            self.assertEqual(json.loads(output_path.read_text())["run"]["id"], "run-1")
            self.assertEqual(calls[0]["url"], "http://127.0.0.1:9800/api/evals/runs")
            self.assertEqual(calls[0]["timeout"], 5)
            self.assertEqual(
                calls[0]["body"],
                {
                    "scenario": "pr_repair",
                    "target": eval_input()["target"],
                    "source_task_id": "task-1",
                },
            )

    def test_persist_eval_score_uploads_input_then_scores_from_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            input_path = tmp_path / "pr_repair_eval_input.json"
            run_path = tmp_path / "eval_run.json"
            artifact_output = tmp_path / "eval_artifact.json"
            score_output = tmp_path / "eval_score.json"
            input_body = json.dumps(eval_input(), indent=2) + "\n"
            input_path.write_text(input_body, encoding="utf-8")
            run_path.write_text(json.dumps({"run": {"id": "run/needs encoding"}}), encoding="utf-8")
            calls: list[dict[str, object]] = []

            def eval_score_urlopen_stub(request: object, timeout: int) -> EvalApiResponseStub:
                body = request.data.decode("utf-8")
                calls.append(
                    {
                        "url": request.full_url,
                        "body": json.loads(body) if body else None,
                        "timeout": timeout,
                    }
                )
                if request.full_url.endswith("/artifacts"):
                    return EvalApiResponseStub(201, {"artifact": {"id": "artifact-1"}})
                return EvalApiResponseStub(
                    200,
                    {
                        "run": {"id": "run/needs encoding", "status": "scored"},
                        "quality_snapshot": {"id": "snapshot-1"},
                    },
                )

            with mock.patch.object(
                MODULE.urllib.request,
                "urlopen",
                side_effect=eval_score_urlopen_stub,
            ):
                status = MODULE.persist_eval_score(
                    argparse.Namespace(
                        server_url="http://127.0.0.1:9800",
                        input=input_path,
                        run=run_path,
                        artifact_output=artifact_output,
                        score_output=score_output,
                    )
                )

            self.assertEqual(status, 0)
            self.assertEqual(
                calls[0]["url"],
                "http://127.0.0.1:9800/api/evals/runs/run%2Fneeds%20encoding/artifacts",
            )
            self.assertEqual(calls[0]["body"]["artifact_type"], "pr_repair_eval_input")
            self.assertEqual(calls[0]["body"]["body"], input_body)
            self.assertEqual(
                calls[1]["url"],
                "http://127.0.0.1:9800/api/evals/runs/run%2Fneeds%20encoding/score",
            )
            self.assertIsNone(calls[1]["body"])
            self.assertEqual(json.loads(score_output.read_text())["quality_snapshot"]["id"], "snapshot-1")


if __name__ == "__main__":
    unittest.main()
