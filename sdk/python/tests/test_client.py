import unittest
from typing import Any

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

    def test_start_thread_omits_cwd_when_not_configured(self) -> None:
        mock = MockRpc()
        harness = Harness(rpc_handler=mock.start_thread_and_run_complete)

        thread = harness.start_thread()

        self.assertEqual(thread.id, "thread-1")
        self.assertEqual(mock.calls[0][0], "thread/start")
        self.assertNotIn("cwd", mock.calls[0][1])

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
        harness = Harness(rpc_handler=mock.run_timeout)
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
        self.assertTrue(any(event["method"] == "sdk:turn/timeout" for event in result.events))

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


if __name__ == "__main__":
    unittest.main()
