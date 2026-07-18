from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
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


HttpHandler = Callable[[str, str, dict[str, Any] | None], Any]
EventHandler = Callable[[Event], None]


class HarnessHttpError(RuntimeError):
    def __init__(self, status: int, message: str, data: Any = None):
        super().__init__(message)
        self.status = status
        self.data = data


class HarnessRpcError(RuntimeError):
    """Deprecated compatibility type for callers importing the old error class."""

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
        http_handler: HttpHandler | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.cwd = cwd
        self.api_token = api_token.strip() if isinstance(api_token, str) else None
        self.headers = dict(headers) if headers else {}
        self.request_timeout_seconds = request_timeout_seconds
        self._http_handler = http_handler

    def start_thread(self, cwd: str | None = None) -> "HarnessThread":
        project = cwd if cwd is not None else self.cwd
        if not isinstance(project, str) or not project:
            raise ValueError(
                "`cwd` is required; pass Harness(cwd=...) or start_thread(cwd=...)."
            )
        return HarnessThread(self, project)

    def resume_thread(self, project: str) -> "HarnessThread":
        return self.thread(project)

    def thread(self, project: str) -> "HarnessThread":
        if not isinstance(project, str) or not project:
            raise ValueError("project handle must not be empty")
        return HarnessThread(self, project)

    def start_turn(self, project: str, prompt: str) -> dict[str, Any]:
        created = self._request(
            "POST",
            "/api/workflows/runtime/submissions",
            {"project": project, "prompt": prompt},
        )
        if not isinstance(created, dict) or created.get("execution_path") != "workflow_runtime":
            raise RuntimeError("server did not create a workflow-runtime submission")
        turn_id = created.get("submission_id") or created.get("task_id")
        if not isinstance(turn_id, str) or not turn_id:
            raise RuntimeError("runtime submission response is missing submission_id")
        return {"turn_id": turn_id}

    def turn_status(
        self,
        turn_id: str,
        timeout_seconds: float | None = None,
        *,
        project: str | None = None,
    ) -> dict[str, Any]:
        encoded_id = urllib.parse.quote(turn_id, safe="")
        detail = self._request(
            "GET",
            f"/api/workflows/runtime/submissions/{encoded_id}",
            None,
            timeout_seconds,
        )
        if not isinstance(detail, dict):
            raise RuntimeError("runtime submission detail must be an object")
        status = _runtime_status_to_turn_status(detail.get("status"))
        items = self._runtime_items(
            turn_id, status, detail.get("error"), timeout_seconds
        )
        turn: dict[str, Any] = {
            "id": detail.get("submission_id") or turn_id,
            "thread_id": detail.get("project") or project or "",
            "status": status,
            "items": items,
        }
        if isinstance(detail.get("created_at"), str):
            turn["started_at"] = detail["created_at"]
        if status != "running" and isinstance(detail.get("updated_at"), str):
            turn["completed_at"] = detail["updated_at"]
        return turn

    def _runtime_items(
        self,
        turn_id: str,
        status: str,
        task_error: Any,
        timeout_seconds: float | None,
    ) -> list[dict[str, Any]]:
        if status == "running":
            return []
        encoded_id = urllib.parse.quote(turn_id, safe="")
        try:
            artifacts = self._request(
                "GET",
                f"/api/workflows/runtime/submissions/{encoded_id}/artifacts",
                None,
                timeout_seconds,
            )
        except HarnessHttpError as error:
            if error.status != 404:
                raise
            artifacts = []
        if not isinstance(artifacts, list):
            raise RuntimeError("runtime artifacts response must be an array")
        final_result: dict[str, Any] = {}
        for artifact in reversed(artifacts):
            if not isinstance(artifact, dict):
                continue
            if artifact.get("artifact_type") != "activity_result_envelope":
                continue
            content = artifact.get("content")
            if not isinstance(content, str):
                continue
            try:
                envelope = json.loads(content)
            except json.JSONDecodeError as error:
                raise RuntimeError(
                    f"Invalid activity_result_envelope artifact: {error}"
                ) from error
            if not isinstance(envelope, dict):
                raise RuntimeError(
                    "Invalid activity_result_envelope artifact: expected an object"
                )
            candidate = envelope.get("final_result")
            if isinstance(candidate, dict):
                final_result = candidate
            break

        items: list[dict[str, Any]] = []
        summary = final_result.get("summary")
        if isinstance(summary, str) and summary.strip():
            items.append({"type": "agent_reasoning", "content": summary})
        error = final_result.get("error")
        if not isinstance(error, str) or not error.strip():
            error = task_error
        if isinstance(error, str) and error.strip():
            items.append({"type": "error", "code": 1, "message": error})
        return items

    def _request(
        self,
        method: str,
        path: str,
        body: dict[str, Any] | None,
        timeout_seconds: float | None = None,
    ) -> Any:
        if self._http_handler is not None:
            return self._http_handler(method, path, body)

        headers = {"Content-Type": "application/json", **self.headers}
        if self.api_token:
            headers["Authorization"] = f"Bearer {self.api_token}"
        request = urllib.request.Request(
            f"{self.base_url}{path}",
            data=json.dumps(body).encode("utf-8") if body is not None else None,
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(
                request,
                timeout=(
                    timeout_seconds
                    if timeout_seconds is not None
                    else self.request_timeout_seconds
                ),
            ) as response:
                raw = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            raw_error = error.read().decode("utf-8", errors="replace")
            try:
                data: Any = json.loads(raw_error)
            except json.JSONDecodeError:
                data = raw_error
            message = (
                str(data.get("error"))
                if isinstance(data, dict) and "error" in data
                else f"HTTP {error.code}"
            )
            raise HarnessHttpError(error.code, message, data) from error
        except urllib.error.URLError as error:
            if isinstance(error.reason, TimeoutError):
                raise TimeoutError("HTTP request timed out") from error
            raise RuntimeError(f"HTTP request failed: {error.reason}") from error
        try:
            return json.loads(raw)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"Invalid HTTP response: {error}") from error


class HarnessThread:
    def __init__(self, client: Harness, project: str) -> None:
        self._client = client
        self.id = project

    def run(
        self,
        prompt: str,
        timeout_seconds: float = 30.0,
        poll_interval_seconds: float = 0.5,
        on_event: EventHandler | None = None,
    ) -> RunResult:
        events: list[Event] = []
        latest_turn: dict[str, Any] | None = None
        turn_id = ""
        for event in self.run_stream(
            prompt,
            poll_interval_seconds=poll_interval_seconds,
            timeout_seconds=timeout_seconds,
            on_event=on_event,
        ):
            events.append(event)
            if event["method"] == "sdk:turn/started":
                turn_id = str(event["params"].get("turn_id", ""))
            elif event["method"] == "sdk:turn/status":
                candidate = event["params"].get("turn")
                if isinstance(candidate, dict):
                    latest_turn = candidate
        timed_out = any(event["method"] == "sdk:turn/timeout" for event in events)
        if latest_turn is None and turn_id and not timed_out:
            latest_turn = self._client.turn_status(turn_id, project=self.id)
        status = str(latest_turn.get("status", "running")) if latest_turn else "running"
        return RunResult(
            thread_id=self.id,
            turn_id=turn_id,
            status=status,
            output=_extract_output(latest_turn),
            turn=latest_turn,
            events=events,
            timed_out=timed_out,
        )

    def run_stream(
        self,
        prompt: str,
        timeout_seconds: float = 30.0,
        poll_interval_seconds: float = 0.5,
        on_event: EventHandler | None = None,
    ) -> Generator[Event, None, None]:
        poll_interval = max(0.01, poll_interval_seconds)
        timeout = max(0.001, timeout_seconds)
        started_at = time.monotonic()
        deadline = started_at + timeout
        previous_signature = ""
        turn_id = str(self._client.start_turn(self.id, prompt)["turn_id"])

        def timeout_event() -> Event:
            return _emit(
                "sdk:turn/timeout",
                {
                    "thread_id": self.id,
                    "turn_id": turn_id,
                    "timeout_ms": int(timeout * 1000),
                    "timeout_seconds": timeout,
                    "source": "sdk-poll",
                    "server_method": "GET /api/workflows/runtime/submissions/{id}",
                },
                on_event,
            )

        yield _emit(
            "sdk:turn/started",
            {
                "thread_id": self.id,
                "turn_id": turn_id,
                "source": "sdk-poll",
                "server_method": "POST /api/workflows/runtime/submissions",
            },
            on_event,
        )
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                yield timeout_event()
                return
            try:
                turn = self._client.turn_status(
                    turn_id,
                    min(self._client.request_timeout_seconds, remaining),
                    project=self.id,
                )
            except TimeoutError:
                yield timeout_event()
                return
            signature = json.dumps(
                [turn.get("status"), turn.get("items"), turn.get("completed_at")],
                sort_keys=True,
            )
            if signature != previous_signature:
                previous_signature = signature
                yield _emit(
                    "sdk:turn/status",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "turn": turn,
                        "source": "sdk-poll",
                        "server_method": "GET /api/workflows/runtime/submissions/{id}",
                    },
                    on_event,
                )
            status = turn.get("status")
            if status in {"completed", "cancelled", "failed"}:
                yield _emit(
                    "sdk:turn/completed",
                    {
                        "thread_id": self.id,
                        "turn_id": turn_id,
                        "status": status,
                        "token_usage": turn.get("token_usage"),
                        "source": "sdk-poll",
                        "server_method": "GET /api/workflows/runtime/submissions/{id}",
                    },
                    on_event,
                )
                return
            if time.monotonic() >= deadline:
                yield timeout_event()
                return
            sleep_for = min(poll_interval, max(0.0, deadline - time.monotonic()))
            if sleep_for > 0:
                time.sleep(sleep_for)


def _runtime_status_to_turn_status(status: Any) -> str:
    if status == "done":
        return "completed"
    if status == "failed":
        return "failed"
    if status == "cancelled":
        return "cancelled"
    return "running"


def _extract_output(turn: dict[str, Any] | None) -> str:
    if not turn or not isinstance(turn.get("items"), list):
        return ""
    messages: list[str] = []
    for item in turn["items"]:
        if not isinstance(item, dict) or item.get("type") == "user_message":
            continue
        for field in ("content", "stdout", "message"):
            value = item.get(field)
            if isinstance(value, str) and value.strip():
                messages.append(value.strip())
                break
    return "\n\n".join(messages)


def _emit(method: EventMethod, params: dict[str, Any], handler: EventHandler | None) -> Event:
    event: Event = {
        "method": method,
        "params": params,
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
    if handler is not None:
        handler(event)
    return event
