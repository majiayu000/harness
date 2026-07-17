from __future__ import annotations

import json
import unittest
import urllib.error
from typing import Any
from unittest import mock

from harness_sdk import Harness, HarnessHttpError


class FakeRuntime:
    def __init__(self) -> None:
        self.calls: list[tuple[str, str, dict[str, Any] | None]] = []
        self.detail_polls = 0

    def __call__(
        self, method: str, path: str, body: dict[str, Any] | None
    ) -> Any:
        self.calls.append((method, path, body))
        if method == "POST" and path == "/api/workflows/runtime/submissions":
            return {
                "task_id": "submission-1",
                "submission_id": "submission-1",
                "execution_path": "workflow_runtime",
            }
        if path == "/api/workflows/runtime/submissions/submission-1":
            self.detail_polls += 1
            return {
                "submission_id": "submission-1",
                "status": "implementing" if self.detail_polls == 1 else "done",
                "project": "/repo",
                "created_at": "2026-07-18T00:00:00Z",
                "updated_at": "2026-07-18T00:00:01Z",
            }
        if path == "/api/workflows/runtime/submissions/submission-1/artifacts":
            return [
                {
                    "artifact_type": "activity_result_envelope",
                    "content": json.dumps(
                        {
                            "final_result": {
                                "summary": "Repository analyzed.",
                                "error": None,
                            }
                        }
                    ),
                }
            ]
        raise AssertionError(f"unexpected request: {method} {path}")


class HarnessSdkTests(unittest.TestCase):
    def test_start_thread_creates_local_project_handle(self) -> None:
        runtime = FakeRuntime()
        harness = Harness(cwd="/repo", http_handler=runtime)

        thread = harness.start_thread()

        self.assertEqual(thread.id, "/repo")
        self.assertEqual(runtime.calls, [])

    def test_start_thread_requires_project_root(self) -> None:
        harness = Harness(http_handler=FakeRuntime())

        with self.assertRaisesRegex(
            ValueError,
            r"`cwd` is required; pass Harness\(cwd=\.\.\.\) or start_thread\(cwd=\.\.\.\)\.",
        ):
            harness.start_thread()

    def test_run_submits_and_polls_runtime_workflow(self) -> None:
        runtime = FakeRuntime()
        harness = Harness(cwd="/repo", http_handler=runtime)
        emitted: list[dict[str, Any]] = []

        result = harness.start_thread().run(
            "Analyze the repository",
            poll_interval_seconds=0.001,
            timeout_seconds=0.5,
            on_event=emitted.append,
        )

        self.assertEqual(result.turn_id, "submission-1")
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.output, "Repository analyzed.")
        self.assertFalse(result.timed_out)
        self.assertTrue(
            any(event["method"] == "sdk:turn/completed" for event in result.events)
        )
        self.assertEqual(emitted, result.events)
        self.assertEqual(
            runtime.calls[0],
            (
                "POST",
                "/api/workflows/runtime/submissions",
                {"project": "/repo", "prompt": "Analyze the repository"},
            ),
        )

    def test_run_exposes_terminal_runtime_error(self) -> None:
        def handler(method: str, path: str, body: dict[str, Any] | None) -> Any:
            del body
            if method == "POST":
                return {
                    "task_id": "submission-failed",
                    "execution_path": "workflow_runtime",
                }
            if path.endswith("/artifacts"):
                return [
                    {
                        "artifact_type": "activity_result_envelope",
                        "content": json.dumps(
                            {
                                "final_result": {
                                    "summary": "Agent failed.",
                                    "error": "spawn failed",
                                }
                            }
                        ),
                    }
                ]
            return {
                "submission_id": "submission-failed",
                "status": "failed",
                "project": "/repo",
            }

        result = Harness(cwd="/repo", http_handler=handler).start_thread().run("Fail")

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.output, "Agent failed.\n\nspawn failed")

    def test_run_times_out_while_submission_is_active(self) -> None:
        calls: list[str] = []

        def handler(method: str, path: str, body: dict[str, Any] | None) -> Any:
            del body
            calls.append(path)
            if method == "POST":
                return {
                    "task_id": "submission-running",
                    "execution_path": "workflow_runtime",
                }
            return {
                "submission_id": "submission-running",
                "status": "implementing",
                "project": "/repo",
            }

        result = Harness(cwd="/repo", http_handler=handler).start_thread().run(
            "Keep going", poll_interval_seconds=0.001, timeout_seconds=0.005
        )

        self.assertEqual(result.status, "running")
        self.assertTrue(result.timed_out)
        self.assertTrue(
            any(event["method"] == "sdk:turn/timeout" for event in result.events)
        )
        self.assertFalse(any(path.endswith("/artifacts") for path in calls))

    def test_run_converts_poll_socket_timeout_to_timeout_event(self) -> None:
        def handler(method: str, path: str, body: dict[str, Any] | None) -> Any:
            del body
            if method == "POST":
                return {
                    "task_id": "submission-socket-timeout",
                    "execution_path": "workflow_runtime",
                }
            raise TimeoutError("socket timed out")

        result = Harness(cwd="/repo", http_handler=handler).start_thread().run(
            "Keep going", timeout_seconds=0.005
        )

        self.assertEqual(result.status, "running")
        self.assertTrue(result.timed_out)
        self.assertEqual(result.events[-1]["method"], "sdk:turn/timeout")

    def test_wrapped_socket_timeout_preserves_timeout_type(self) -> None:
        harness = Harness(base_url="http://127.0.0.1:9800")

        with mock.patch(
            "urllib.request.urlopen",
            side_effect=urllib.error.URLError(TimeoutError("socket timed out")),
        ):
            with self.assertRaisesRegex(TimeoutError, "HTTP request timed out"):
                harness.turn_status("submission-timeout")

    def test_resume_thread_reuses_project_handle_locally(self) -> None:
        runtime = FakeRuntime()
        harness = Harness(http_handler=runtime)

        thread = harness.resume_thread("/repo")

        self.assertEqual(thread.id, "/repo")
        self.assertEqual(runtime.calls, [])

    def test_http_error_preserves_status_and_data(self) -> None:
        payload = {"error": "workflow runtime store unavailable"}
        error = HarnessHttpError(503, payload["error"], payload)
        self.assertEqual(error.status, 503)
        self.assertEqual(error.data, payload)

    def test_bearer_token_and_custom_headers_are_sent(self) -> None:
        captured_headers: dict[str, str] = {}

        class FakeResponse:
            def __enter__(self) -> "FakeResponse":
                return self

            def __exit__(self, exc_type, exc, tb) -> None:
                return None

            def read(self) -> bytes:
                return json.dumps(
                    {
                        "task_id": "submission-auth",
                        "execution_path": "workflow_runtime",
                    }
                ).encode()

        def fake_urlopen(request: Any, timeout: float) -> FakeResponse:
            del timeout
            captured_headers.update(dict(request.header_items()))
            return FakeResponse()

        harness = Harness(
            base_url="http://127.0.0.1:9800",
            cwd="/repo",
            api_token="token-123",
            headers={"X-Test-Header": "enabled"},
        )

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            harness.start_turn("/repo", "Run")

        normalized = {key.lower(): value for key, value in captured_headers.items()}
        self.assertEqual(normalized.get("authorization"), "Bearer token-123")
        self.assertEqual(normalized.get("x-test-header"), "enabled")

    def test_malformed_runtime_result_artifact_fails_loudly(self) -> None:
        def handler(method: str, path: str, body: dict[str, Any] | None) -> Any:
            del body
            if method == "POST":
                return {
                    "task_id": "submission-bad",
                    "execution_path": "workflow_runtime",
                }
            if path.endswith("/artifacts"):
                return [
                    {
                        "artifact_type": "activity_result_envelope",
                        "content": "not-json",
                    }
                ]
            return {
                "submission_id": "submission-bad",
                "status": "done",
                "project": "/repo",
            }

        harness = Harness(cwd="/repo", http_handler=handler)

        with self.assertRaisesRegex(
            RuntimeError, "Invalid activity_result_envelope artifact"
        ):
            harness.start_thread().run("Run")


if __name__ == "__main__":
    unittest.main()
