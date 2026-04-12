from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Callable, Generator, Literal, TypedDict


EventMethod = Literal[
    "sdk:turn/started",
    "sdk:turn/status",
    "sdk:turn/completed",
    "sdk:turn/timeout",
]


class Event(TypedDict):
    method: EventMethod
    params: dict[str, Any]
    timestamp: str


RpcHandler = Callable[[str, dict[str, Any]], Any]
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
    turn: dict[str, Any] | None
    events: list[Event]
    timed_out: bool


class Harness:
    def __init__(
        self,
        base_url: str = "http://127.0.0.1:9800",
        cwd: str | None = None,
        api_token: str | None = None,
        headers: dict[str, str] | None = None,
        request_timeout_seconds: float = 15.0,
        rpc_handler: RpcHandler | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.cwd = cwd
        self.api_token = api_token.strip() if isinstance(api_token, str) else None
        self.headers = dict(headers) if headers else {}
        self.request_timeout_seconds = request_timeout_seconds
        self._rpc_handler = rpc_handler
        self._next_id = 1

    def start_thread(self, cwd: str | None = None) -> "HarnessThread":
        resolved_cwd = cwd if cwd is not None else self.cwd
        if not isinstance(resolved_cwd, str) or not resolved_cwd:
            raise ValueError(
                "`cwd` is required for thread/start; pass Harness(cwd=...) "
                "or start_thread(cwd=...)."
            )
        params: dict[str, Any] = {"cwd": resolved_cwd}

        result = self._rpc("thread/start", params)
        return HarnessThread(self, str(result["thread_id"]))

    def resume_thread(self, thread_id: str) -> "HarnessThread":
        self._rpc("thread/resume", {"thread_id": thread_id})
        return HarnessThread(self, thread_id)

    def thread(self, thread_id: str) -> "HarnessThread":
        return HarnessThread(self, thread_id)

    def start_turn(self, thread_id: str, prompt: str) -> dict[str, Any]:
        return self._rpc("turn/start", {"thread_id": thread_id, "input": prompt})

    def turn_status(
        self, turn_id: str, timeout_seconds: float | None = None
    ) -> dict[str, Any]:
        return self._rpc(
            "turn/status",
            {"turn_id": turn_id},
            timeout_seconds=timeout_seconds,
        )

    def _rpc(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        timeout_seconds: float | None = None,
    ) -> Any:
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
        headers = {"Content-Type": "application/json", **self.headers}
        if self.api_token:
            headers["Authorization"] = f"Bearer {self.api_token}"
        request = urllib.request.Request(
            f"{self.base_url}/rpc",
            data=body,
            method="POST",
            headers=headers,
        )

        try:
            timeout = (
                self.request_timeout_seconds
                if timeout_seconds is None
                else max(0.01, float(timeout_seconds))
            )
            with urllib.request.urlopen(request, timeout=timeout) as response:
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
        on_event: EventHandler | None = None,
    ) -> RunResult:
        events: list[Event] = []
        turn_id = ""
        latest_turn: dict[str, Any] | None = None

        for event in self.run_stream(
            prompt,
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval_seconds,
            on_event=on_event,
        ):
            events.append(event)
            if event["method"] == "sdk:turn/started":
                turn_id = str(event["params"]["turn_id"])
            if event["method"] == "sdk:turn/status":
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
            timed_out=any(event["method"] == "sdk:turn/timeout" for event in events),
        )

    def run_stream(
        self,
        prompt: str,
        timeout_seconds: float = 30.0,
        poll_interval_seconds: float = 0.5,
        on_event: EventHandler | None = None,
    ) -> Generator[Event, None, None]:
        poll_interval = max(0.01, float(poll_interval_seconds))
        timeout = max(0.01, float(timeout_seconds))
        deadline = time.monotonic() + timeout

        started = self._client.start_turn(self.id, prompt)
        turn_id = str(started["turn_id"])
        previous_signature = ""

        start_event = _new_event(
            "sdk:turn/started",
            {
                "thread_id": self.id,
                "turn_id": turn_id,
                "source": "sdk-poll",
                "server_method": "turn/start",
            },
        )
        _notify(on_event, start_event)
        yield start_event

        while True:
            remaining_before_poll = deadline - time.monotonic()
            if remaining_before_poll <= 0:
                timeout_event = _new_event(
                    "sdk:turn/timeout",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "timeout_ms": int(timeout * 1000),
                        "timeout_seconds": timeout,
                        "source": "sdk-poll",
                        "server_method": "turn/status",
                    },
                )
                _notify(on_event, timeout_event)
                yield timeout_event
                return

            turn = self._client.turn_status(
                turn_id,
                timeout_seconds=min(
                    self._client.request_timeout_seconds, remaining_before_poll
                ),
            )
            signature = _turn_signature(turn)

            if signature != previous_signature:
                previous_signature = signature
                status_event = _new_event(
                    "sdk:turn/status",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "turn": turn,
                        "source": "sdk-poll",
                        "server_method": "turn/status",
                    },
                )
                _notify(on_event, status_event)
                yield status_event

            status = str(turn.get("status", "running"))
            if _is_terminal_status(status):
                completed_event = _new_event(
                    "sdk:turn/completed",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "status": status,
                        "token_usage": turn.get("token_usage"),
                        "source": "sdk-poll",
                        "server_method": "turn/status",
                    },
                )
                _notify(on_event, completed_event)
                yield completed_event
                return

            if time.monotonic() >= deadline:
                timeout_event = _new_event(
                    "sdk:turn/timeout",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "timeout_ms": int(timeout * 1000),
                        "timeout_seconds": timeout,
                        "source": "sdk-poll",
                        "server_method": "turn/status",
                    },
                )
                _notify(on_event, timeout_event)
                yield timeout_event
                return

            sleep_for = min(poll_interval, max(0.0, deadline - time.monotonic()))
            if sleep_for > 0:
                time.sleep(sleep_for)


def _notify(on_event: EventHandler | None, event: Event) -> None:
    if on_event:
        on_event(event)


def _new_event(method: EventMethod, params: dict[str, Any]) -> Event:
    timestamp = (
        datetime.now(timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )
    return {
        "method": method,
        "params": params,
        "timestamp": timestamp,
    }


def _turn_signature(turn: dict[str, Any]) -> str:
    status = str(turn.get("status", "running"))
    items = turn.get("items")
    item_count = len(items) if isinstance(items, list) else 0
    completed_at = str(turn.get("completed_at") or "")
    return f"{status}|{item_count}|{completed_at}"


def _is_terminal_status(status: str) -> bool:
    return status in {"completed", "cancelled", "failed"}


def _extract_output(turn: dict[str, Any] | None) -> str:
    if not turn:
        return ""

    items = turn.get("items")
    if not isinstance(items, list):
        return ""

    messages: list[str] = []
    for item in items:
        if not isinstance(item, dict):
            continue

        if item.get("type") == "user_message":
            continue

        for key in ("content", "stdout", "message"):
            value = item.get(key)
            if isinstance(value, str) and value.strip():
                messages.append(value.strip())
                break

    return "\n\n".join(messages)
