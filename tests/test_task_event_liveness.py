from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CHECKS = ROOT / "checks"
sys.path.insert(0, str(CHECKS))

from task_event_liveness import audit_event_log  # noqa: E402


def write_events(path: Path, events: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(event) + "\n" for event in events),
        encoding="utf-8",
    )


def test_liveness_audit_passes_terminalized_stream(tmp_path: Path) -> None:
    log = tmp_path / "task-events.jsonl"
    write_events(
        log,
        [
            {"type": "created", "task_id": "task-1", "ts": 1},
            {"type": "status_changed", "task_id": "task-1", "ts": 2, "status": "waiting", "turn": 1},
            {"type": "failed", "task_id": "task-1", "ts": 3, "reason": "round_budget_exhausted"},
            {"type": "created", "task_id": "task-2", "ts": 4},
            {"type": "completed", "task_id": "task-2", "ts": 5},
            {"type": "created", "task_id": "task-3", "ts": 6},
            {"type": "cancelled", "task_id": "task-3", "ts": 7, "reason": "operator"},
        ],
    )

    result = audit_event_log(log)

    assert result["ok"] is True
    assert result["created_events"] == 3
    assert result["terminal_events"] == 3


def test_liveness_audit_fails_synthetic_limbo_fixture(tmp_path: Path) -> None:
    log = tmp_path / "task-events.jsonl"
    write_events(
        log,
        [
            {"type": "created", "task_id": "limbo-task", "ts": 1},
            {
                "type": "status_changed",
                "task_id": "limbo-task",
                "ts": 2,
                "status": "waiting",
                "turn": 10,
            },
        ],
    )

    result = audit_event_log(log)

    assert result["ok"] is False
    assert result["missing_terminal"][0]["task_id"] == "limbo-task"


def test_liveness_audit_fails_duplicate_terminal(tmp_path: Path) -> None:
    log = tmp_path / "task-events.jsonl"
    write_events(
        log,
        [
            {"type": "created", "task_id": "double-terminal", "ts": 1},
            {"type": "failed", "task_id": "double-terminal", "ts": 2, "reason": "first"},
            {"type": "completed", "task_id": "double-terminal", "ts": 3},
        ],
    )

    result = audit_event_log(log)

    assert result["ok"] is False
    assert result["duplicate_terminal"][0]["task_id"] == "double-terminal"


def test_liveness_audit_cli_returns_nonzero_for_limbo(tmp_path: Path) -> None:
    log = tmp_path / "task-events.jsonl"
    write_events(log, [{"type": "created", "task_id": "limbo-task", "ts": 1}])

    result = subprocess.run(
        [sys.executable, "checks/task_event_liveness.py", str(log), "--json"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    payload = json.loads(result.stdout)
    assert payload["missing_terminal"][0]["task_id"] == "limbo-task"
