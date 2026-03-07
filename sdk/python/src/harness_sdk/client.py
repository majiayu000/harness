from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Dict, Generator, List, Optional

Event = Dict[str, Any]
RpcHandler = Callable[[str, Dict[str, Any]], Any]
EventHandler = Callable[[Event], None]


class HarnessRpcError(RuntimeError):
    def __init__(self, code: int, message: str, data: Any = None):
        super().__init__(message)
        self.code = code
        self.data = data


@dataclass
class RunResult:
    thread_id: str
    turn_id: str
    status: str
    output: str
    turn: Optional[Dict[str, Any]]
    events: List[Event]
    timed_out: bool


class Harness:
    def __init__(
        self,
        base_url: str = "http://127.0.0.1:8080",
        cwd: Optional[str] = None,
        request_timeout_seconds: float = 15.0,
        rpc_handler: Optional[RpcHandler] = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.cwd = cwd or os.getcwd()
        self.request_timeout_seconds = request_timeout_seconds
        self._rpc_handler = rpc_handler
        self._next_id = 1

    def start_thread(self, cwd: Optional[str] = None) -> "HarnessThread":
        result = self._rpc("thread/start", {"cwd": cwd or self.cwd})
        return HarnessThread(self, str(result["thread_id"]))

    def resume_thread(self, thread_id: str) -> "HarnessThread":
        self._rpc("thread/resume", {"thread_id": thread_id})
        return HarnessThread(self, thread_id)

    def thread(self, thread_id: str) -> "HarnessThread":
        return HarnessThread(self, thread_id)

    def start_turn(self, thread_id: str, prompt: str) -> Dict[str, Any]:
        return self._rpc("turn/start", {"thread_id": thread_id, "input": prompt})

    def turn_status(self, turn_id: str) -> Dict[str, Any]:
        return self._rpc("turn/status", {"turn_id": turn_id})

    def _rpc(self, method: str, params: Optional[Dict[str, Any]] = None) -> Any:
        if self._rpc_handler:
            return self._rpc_handler(method, params or {})

        request_id = self._next_id
        self._next_id += 1

        payload = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        }
        body = json.dumps(payload).encode("utf-8")
        request = urllib.request.Request(
            f"{self.base_url}/rpc",
            data=body,
            method="POST",
            headers={"Content-Type": "application/json"},
        )

        try:
            with urllib.request.urlopen(
                request, timeout=self.request_timeout_seconds
            ) as response:
                response_body = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            response_body = error.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"HTTP {error.code}: {response_body}") from error
        except urllib.error.URLError as error:
            raise RuntimeError(f"Failed to reach Harness server: {error}") from error

        try:
            parsed = json.loads(response_body)
        except json.JSONDecodeError as error:
            raise RuntimeError(
                f"Failed to parse RPC response for '{method}': {error}"
            ) from error

        if parsed.get("error"):
            rpc_error = parsed["error"]
            raise HarnessRpcError(
                code=int(rpc_error.get("code", -32000)),
                message=str(rpc_error.get("message", "unknown error")),
                data=rpc_error.get("data"),
            )

        if "result" not in parsed:
            raise RuntimeError(f"Missing RPC result for '{method}'")
        return parsed["result"]


class HarnessThread:
    def __init__(self, client: Harness, thread_id: str) -> None:
        self._client = client
        self.id = thread_id

    def run(
        self,
        prompt: str,
        timeout_seconds: float = 30.0,
        poll_interval_seconds: float = 0.5,
        on_event: Optional[EventHandler] = None,
    ) -> RunResult:
        events: List[Event] = []
        turn_id = ""
        latest_turn: Optional[Dict[str, Any]] = None

        for event in self.run_stream(
            prompt,
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval_seconds,
            on_event=on_event,
        ):
            events.append(event)
            if event["method"] == "turn/started":
                turn_id = str(event["params"]["turn_id"])
            if event["method"] == "turn/status":
                latest_turn = event["params"]["turn"]

        if not latest_turn and turn_id:
            latest_turn = self._client.turn_status(turn_id)

        return RunResult(
            thread_id=self.id,
            turn_id=turn_id,
            status=str((latest_turn or {}).get("status", "running")),
            output=_extract_output(latest_turn),
            turn=latest_turn,
            events=events,
            timed_out=any(event["method"] == "turn/timeout" for event in events),
        )

    def run_stream(
        self,
        prompt: str,
        timeout_seconds: float = 30.0,
        poll_interval_seconds: float = 0.5,
        on_event: Optional[EventHandler] = None,
    ) -> Generator[Event, None, None]:
        poll_interval = max(0.01, float(poll_interval_seconds))
        timeout = max(0.01, float(timeout_seconds))

        started = self._client.start_turn(self.id, prompt)
        turn_id = str(started["turn_id"])
        started_at = time.monotonic()
        previous_signature = ""

        start_event = _new_event(
            "turn/started",
            {"thread_id": self.id, "turn_id": turn_id, "source": "rpc"},
        )
        _notify(on_event, start_event)
        yield start_event

        while True:
            turn = self._client.turn_status(turn_id)
            signature = _turn_signature(turn)

            if signature != previous_signature:
                previous_signature = signature
                status_event = _new_event(
                    "turn/status",
                    {"thread_id": self.id, "turn_id": turn_id, "turn": turn},
                )
                _notify(on_event, status_event)
                yield status_event

            status = str(turn.get("status", "running"))
            if _is_terminal_status(status):
                completed_event = _new_event(
                    "turn/completed",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "status": status,
                        "token_usage": turn.get("token_usage"),
                    },
                )
                _notify(on_event, completed_event)
                yield completed_event
                return

            if time.monotonic() - started_at >= timeout:
                timeout_event = _new_event(
                    "turn/timeout",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "timeout_seconds": timeout,
                    },
                )
                _notify(on_event, timeout_event)
                yield timeout_event
                return

            time.sleep(poll_interval)


def _notify(on_event: Optional[EventHandler], event: Event) -> None:
    if on_event:
        on_event(event)


def _new_event(method: str, params: Dict[str, Any]) -> Event:
    return {
        "method": method,
        "params": params,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def _turn_signature(turn: Dict[str, Any]) -> str:
    status = str(turn.get("status", "running"))
    items = turn.get("items")
    item_count = len(items) if isinstance(items, list) else 0
    completed_at = str(turn.get("completed_at") or "")
    return f"{status}|{item_count}|{completed_at}"


def _is_terminal_status(status: str) -> bool:
    return status in {"completed", "cancelled", "failed"}


def _extract_output(turn: Optional[Dict[str, Any]]) -> str:
    if not turn:
        return ""

    items = turn.get("items")
    if not isinstance(items, list):
        return ""

    messages: List[str] = []
    for item in items:
        if not isinstance(item, dict):
            continue

        if item.get("type") == "user_message":
            continue

        content = item.get("content")
        if isinstance(content, str) and content.strip():
            messages.append(content.strip())
            continue

        stdout = item.get("stdout")
        if isinstance(stdout, str) and stdout.strip():
            messages.append(stdout.strip())
            continue

        message = item.get("message")
        if isinstance(message, str) and message.strip():
            messages.append(message.strip())

    return "\n\n".join(messages)
