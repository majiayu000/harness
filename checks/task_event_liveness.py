#!/usr/bin/env python3
"""Audit task-events.jsonl for created-task terminal liveness."""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


TERMINAL_EVENT_TYPES = {"completed", "failed", "cancelled"}


@dataclass
class TaskEventCounts:
    created: int = 0
    terminal: int = 0
    terminal_types: list[str] = field(default_factory=list)


def _issue_dict(task_id: str, counts: TaskEventCounts) -> dict[str, Any]:
    return {
        "task_id": task_id,
        "created": counts.created,
        "terminal": counts.terminal,
        "terminal_types": counts.terminal_types,
    }


def audit_event_log(log_path: Path) -> dict[str, Any]:
    tasks: dict[str, TaskEventCounts] = {}
    invalid_lines: list[dict[str, Any]] = []
    total_lines = 0

    if not log_path.exists():
        return {
            "ok": True,
            "log": str(log_path),
            "missing_log": True,
            "created_events": 0,
            "terminal_events": 0,
            "task_count": 0,
            "missing_terminal": [],
            "terminal_without_created": [],
            "duplicate_created": [],
            "duplicate_terminal": [],
            "invalid_lines": [],
        }

    with log_path.open("r", encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, start=1):
            stripped = line.strip()
            if not stripped:
                continue
            total_lines += 1
            try:
                event = json.loads(stripped)
            except json.JSONDecodeError as error:
                invalid_lines.append({"line": line_no, "error": str(error)})
                continue

            event_type = event.get("type")
            task_id = event.get("task_id")
            if not isinstance(event_type, str) or not isinstance(task_id, str) or not task_id:
                invalid_lines.append(
                    {
                        "line": line_no,
                        "error": "event must contain string fields type and task_id",
                    }
                )
                continue

            counts = tasks.setdefault(task_id, TaskEventCounts())
            if event_type == "created":
                counts.created += 1
            elif event_type in TERMINAL_EVENT_TYPES:
                counts.terminal += 1
                counts.terminal_types.append(event_type)

    missing_terminal = [
        _issue_dict(task_id, counts)
        for task_id, counts in sorted(tasks.items())
        if counts.created > 0 and counts.terminal == 0
    ]
    terminal_without_created = [
        _issue_dict(task_id, counts)
        for task_id, counts in sorted(tasks.items())
        if counts.created == 0 and counts.terminal > 0
    ]
    duplicate_created = [
        _issue_dict(task_id, counts)
        for task_id, counts in sorted(tasks.items())
        if counts.created > 1
    ]
    duplicate_terminal = [
        _issue_dict(task_id, counts)
        for task_id, counts in sorted(tasks.items())
        if counts.terminal > 1
    ]
    created_events = sum(counts.created for counts in tasks.values())
    terminal_events = sum(counts.terminal for counts in tasks.values())
    ok = not (
        invalid_lines
        or missing_terminal
        or terminal_without_created
        or duplicate_created
        or duplicate_terminal
    )

    return {
        "ok": ok,
        "log": str(log_path),
        "missing_log": False,
        "total_lines": total_lines,
        "created_events": created_events,
        "terminal_events": terminal_events,
        "task_count": len(tasks),
        "missing_terminal": missing_terminal,
        "terminal_without_created": terminal_without_created,
        "duplicate_created": duplicate_created,
        "duplicate_terminal": duplicate_terminal,
        "invalid_lines": invalid_lines,
    }


def _format_failure(result: dict[str, Any]) -> str:
    parts = [
        f"task event liveness audit failed for {result['log']}",
        f"created_events={result['created_events']}",
        f"terminal_events={result['terminal_events']}",
    ]
    for key in [
        "missing_terminal",
        "terminal_without_created",
        "duplicate_created",
        "duplicate_terminal",
        "invalid_lines",
    ]:
        values = result.get(key) or []
        if values:
            parts.append(f"{key}={values}")
    return "; ".join(parts)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Assert each created task in task-events.jsonl has exactly one terminal event."
    )
    parser.add_argument("log", nargs="?", default="task-events.jsonl")
    parser.add_argument("--json", action="store_true", help="print machine-readable JSON")
    args = parser.parse_args(argv)

    result = audit_event_log(Path(args.log))
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    elif result["ok"]:
        print(
            "task event liveness audit passed: "
            f"created_events={result['created_events']} "
            f"terminal_events={result['terminal_events']}"
        )
    else:
        print(_format_failure(result), file=sys.stderr)
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
