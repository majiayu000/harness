import unittest
from typing import Any
from unittest import mock

try:
    from harness_sdk import Harness, HarnessRpcError
    from harness_sdk.client import _extract_output
except ModuleNotFoundError:
    from sdk.python.src.harness_sdk import Harness, HarnessRpcError
    from sdk.python.src.harness_sdk.client import _extract_output


class MockRpc:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []
        self.status_polls = 0

    def start_thread_and_run_complete(self, method: str, params: dict[str, Any]) -> Any:
        self.calls.append((method, params))
        if method == "thread/start":
            return {"thread_id": "thread-1"}
        if method == "turn/start":
            return {"turn_id": "turn-1"}
        if method == "turn/status":
            self.status_polls += 1
            if self.status_polls == 1:
                return {
                    "id": "turn-1",
                    "thread_id": "thread-1",
                    "status": "running",
                    "items": [{"type": "user_message", "content": "hello"}],
                }
            return {
                "id": "turn-1",
                "thread_id": "thread-1",
                "status": "completed",
                "items": [
                    {"type": "user_message", "content": "hello"},
                    {"type": "agent_reasoning", "content": "done"},
                ],
            }
        if method == "thread/resume":
            return {"resumed": True}
        raise AssertionError(f"unexpected RPC method: {method}")

    def run_timeout(self, method: str, params: dict[str, Any]) -> Any:
        self.calls.append((method, params))
        if method == "thread/start":
            return {"thread_id": "thread-2"}
        if method == "turn/start":
            return {"turn_id": "turn-2"}
        if method == "turn/status":
            return {
                "id": "turn-2",
                "thread_id": "thread-2",
                "status": "running",
                "items": [{"type": "user_message", "content": "still running"}],
            }
        raise AssertionError(f"unexpected RPC method: {method}")

    def rpc_error(self, method: str, params: dict[str, Any]) -> Any:
        self.calls.append((method, params))
        raise HarnessRpcError(-32001, "thread not found")


class HarnessSdkTests(unittest.TestCase):
    def test_start_thread_uses_thread_start_rpc(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.start_thread_and_run_complete, cwd="/repo")

        thread = harness.start_thread()

        self.assertEqual(thread.id, "thread-1")
        self.assertEqual(mock.calls[0][0], "thread/start")
        self.assertEqual(mock.calls[0][1]["cwd"], "/repo")

    def test_start_thread_requires_cwd_when_not_configured(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.start_thread_and_run_complete)

        with self.assertRaisesRegex(
            ValueError,
            r"`cwd` is required for thread/start; pass Harness\(cwd=\.\.\.\) or start_thread\(cwd=\.\.\.\)\.",
        ):
            harness.start_thread()
        self.assertEqual(mock.calls, [])

    def test_run_collects_events_and_output(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.start_thread_and_run_complete)
        thread = harness.start_thread(cwd="/repo")
        emitted: list[dict[str, Any]] = []

        result = thread.run(
            "Summarize repository",
            timeout_seconds=1.0,
            poll_interval_seconds=0.01,
            on_event=lambda event: emitted.append(event),
        )

        self.assertEqual(result.thread_id, "thread-1")
        self.assertEqual(result.turn_id, "turn-1")
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.output, "done")
        self.assertFalse(result.timed_out)
        self.assertTrue(
            any(event["method"] == "sdk:turn/completed" for event in result.events)
        )
        self.assertGreaterEqual(len(emitted), 3)

    def test_run_returns_timeout_when_turn_never_completes(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.run_timeout, cwd="/repo")
        thread = harness.start_thread()

        result = thread.run(
            "Keep going",
            timeout_seconds=0.05,
            poll_interval_seconds=0.01,
        )

        self.assertEqual(result.thread_id, "thread-2")
        self.assertEqual(result.turn_id, "turn-2")
        self.assertEqual(result.status, "running")
        self.assertTrue(result.timed_out)
        timeout_event = next(
            event for event in result.events if event["method"] == "sdk:turn/timeout"
        )
        self.assertEqual(timeout_event["params"]["timeout_seconds"], 0.05)

    def test_extract_output_handles_multiple_item_shapes(self) -> None:
        turn = {
            "items": [
                {"type": "user_message", "content": "ignored"},
                {"type": "agent_reasoning", "content": "done"},
                {"type": "shell_command", "stdout": "ls output"},
                {"type": "error", "message": "tool failed"},
            ]
        }

        self.assertEqual(_extract_output(turn), "done\n\nls output\n\ntool failed")

    def test_resume_thread_surfaces_rpc_errors(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.rpc_error)

        with self.assertRaises(HarnessRpcError):
            harness.resume_thread("missing-thread")

    def test_start_thread_sends_bearer_token_header_when_configured(self) -> None:
        captured_headers: dict[str, str] = {}

        class FakeResponse:
            def __enter__(self) -> "FakeResponse":
                return self

            def __exit__(self, exc_type, exc, tb) -> None:
                return None

            def read(self) -> bytes:
                return b'{"jsonrpc":"2.0","id":1,"result":{"thread_id":"thread-auth"}}'

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
            thread = harness.start_thread()

        normalized = {k.lower(): v for k, v in captured_headers.items()}
        self.assertEqual(thread.id, "thread-auth")
        self.assertEqual(normalized.get("authorization"), "Bearer token-123")
        self.assertEqual(normalized.get("x-test-header"), "enabled")


if __name__ == "__main__":
    unittest.main()
