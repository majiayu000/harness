#!/usr/bin/env python3
"""Tests for the PR repair task submission helper."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_PATH = Path(__file__).with_name("evaluate_pr_repair_submit.py")
SPEC = importlib.util.spec_from_file_location("evaluate_pr_repair_submit", SCRIPT_PATH)
assert SPEC is not None
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class SubmitHelperTests(unittest.TestCase):
    def test_network_errors_write_failed_submission_artifact(self) -> None:
        cases = [
            (TimeoutError("timed out"), "timed out"),
            (OSError("socket closed"), "socket closed"),
        ]
        for exc, expected_error in cases:
            with self.subTest(error=expected_error):
                with tempfile.TemporaryDirectory() as tmp:
                    tmp_path = Path(tmp)
                    body_path = tmp_path / "body.json"
                    output_path = tmp_path / "submission.json"
                    body_path.write_text('{"prompt":"fix it"}\n', encoding="utf-8")

                    argv = [
                        "evaluate_pr_repair_submit.py",
                        "--server-url",
                        "http://127.0.0.1:1",
                        "--body",
                        str(body_path),
                        "--output",
                        str(output_path),
                        "--mode",
                        "prompt_task",
                    ]
                    with mock.patch.object(sys, "argv", argv):
                        with mock.patch.object(
                            MODULE.urllib.request,
                            "urlopen",
                            side_effect=exc,
                        ):
                            status = MODULE.main()

                    self.assertEqual(status, 7)
                    self.assertEqual(
                        json.loads(output_path.read_text(encoding="utf-8")),
                        {
                            "error": expected_error,
                            "eval_submission_mode": "prompt_task",
                            "http_status": "000",
                            "status": "failed",
                        },
                    )


if __name__ == "__main__":
    unittest.main()
