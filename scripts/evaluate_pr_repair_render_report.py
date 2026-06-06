#!/usr/bin/env python3
"""Render a Markdown PR repair eval report from collected artifacts."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load_json(path: Path, *, required: bool = False) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        if required:
            raise
        return {}
    return data if isinstance(data, dict) else {}


def pr_facts(pr: dict[str, Any]) -> dict[str, Any]:
    review_threads = pr.get("reviewThreads")
    thread_nodes = review_threads.get("nodes") if isinstance(review_threads, dict) else None
    threads = [thread for thread in (thread_nodes or []) if isinstance(thread, dict)]
    files_container = pr.get("files")
    file_nodes = files_container.get("nodes") if isinstance(files_container, dict) else None
    files = [item for item in (file_nodes or []) if isinstance(item, dict)]
    status_check_rollup = pr.get("statusCheckRollup")
    active = [
        thread
        for thread in threads
        if not thread.get("isResolved", False) and not thread.get("isOutdated", False)
    ]
    return {
        "head": pr.get("headRefOid"),
        "merge": pr.get("mergeStateStatus") or "UNKNOWN",
        "checks": (
            status_check_rollup.get("state") if isinstance(status_check_rollup, dict) else None
        )
        or "UNKNOWN",
        "unresolved": len(active),
        "files_collected": isinstance(files_container, dict),
        "changed_files": files,
        "changed_file_paths": sorted(
            {
                item.get("path")
                for item in files
                if item.get("path")
            }
        ),
    }


def markdown_cell(value: object) -> str:
    return str(value).replace("|", "\\|")


def compact(value: object, limit: int = 240) -> str:
    text = str(value).replace("\n", " ").strip()
    if len(text) <= limit:
        return text
    return f"{text[:limit - 3]}..."


def file_rows(files: list[dict[str, Any]]) -> list[tuple[str, str, int, int]]:
    rows = []
    valid_files = [item for item in files if isinstance(item, dict)]
    for item in sorted(valid_files, key=lambda row: row.get("path") or ""):
        rows.append(
            (
                item.get("path") or "UNKNOWN",
                item.get("changeType") or "UNKNOWN",
                item.get("additions", 0),
                item.get("deletions", 0),
            )
        )
    return rows


def limited_path_list(paths: list[str]) -> str:
    if not paths:
        return ""
    shown = ", ".join(paths[:5])
    if len(paths) > 5:
        return f"{shown}, ... ({len(paths)} total)"
    return shown


def candidate_class(before: dict[str, Any]) -> str:
    if before["unresolved"] > 0:
        return "review_feedback_repair"
    if before["checks"] != "SUCCESS":
        return "ci_repair"
    if before["merge"] == "CLEAN" and before["checks"] == "SUCCESS":
        return "ready_noop"
    return "mergeability_repair"


def render_report(args: argparse.Namespace) -> str:
    baseline = load_json(args.baseline)
    final = load_json(args.final)
    submission = load_json(args.submission)
    task_detail = load_json(args.task_detail)
    quality_snapshot = load_json(args.quality_snapshot, required=True)

    before = pr_facts(baseline)
    after = pr_facts(final)
    head_changed = before["head"] != after["head"]
    new_changed_paths = sorted(set(after["changed_file_paths"]) - set(before["changed_file_paths"]))
    removed_changed_paths = sorted(
        set(before["changed_file_paths"]) - set(after["changed_file_paths"])
    )
    workflow = task_detail.get("workflow")
    workflow_dict = workflow if isinstance(workflow, dict) else {}
    task_status = (
        task_detail.get("status")
        or workflow_dict.get("state")
        or submission.get("status")
        or "unknown"
    )
    workflow_id = submission.get("workflow_id") or workflow_dict.get("id")
    task_id = submission.get("task_id") or task_detail.get("id")
    submission_mode = submission.get("eval_submission_mode") or "unknown"
    submission_http_status = submission.get("http_status") or task_detail.get("http_status")
    submission_error = (
        submission.get("error")
        or submission.get("message")
        or submission.get("detail")
        or submission.get("raw_response")
        or task_detail.get("error")
    )

    snapshot_blockers = quality_snapshot.get("blocker_summary")
    if not isinstance(snapshot_blockers, list):
        snapshot_blockers = []
    hard_gates = quality_snapshot.get("hard_gates")
    if not isinstance(hard_gates, list):
        hard_gates = []
    failed_gates = [
        gate for gate in hard_gates if isinstance(gate, dict) and gate.get("status") == "fail"
    ]
    scenario = quality_snapshot.get("scenario") or candidate_class(before)

    lines = [
        "# PR Repair Evaluation",
        "",
        f"- Run ID: `{args.run_id}`",
        f"- Repo: `{args.repo}`",
        f"- PR: `#{args.pr}`",
        f"- Server URL: `{args.server_url}`",
        f"- Scenario: `{scenario}`",
        f"- Task ID: `{task_id}`",
        f"- Workflow ID: `{workflow_id}`",
        f"- Task status: `{task_status}`",
        f"- Submission mode: `{submission_mode}`",
    ]
    if submission_http_status:
        lines.append(f"- Submission HTTP status: `{submission_http_status}`")
    if submission_error:
        lines.append(f"- Submission error: `{markdown_cell(compact(submission_error))}`")
    lines.extend(
        [
            f"- Timed out: `{str(args.timed_out).lower()}`",
            f"- Grade: `{quality_snapshot.get('final_grade')}`",
        ]
    )
    if quality_snapshot.get("final_score") is not None:
        lines.append(f"- Score: `{quality_snapshot.get('final_score')}`")
    lines.extend(
        [
            f"- Grade cap: `{quality_snapshot.get('grade_cap') or 'none'}`",
            "",
            "## Eval Bounds",
            "",
            "| Field | Value |",
            "|---|---|",
            f"| `wait_secs` | `{args.wait_secs}` |",
            f"| `max_rounds` | `{args.max_rounds}` |",
            f"| `max_turns` | `{args.max_turns}` |",
            f"| `max_budget_usd` | `{args.max_budget_usd or 'not set'}` |",
            f"| `timeout_secs` | `{args.timeout_secs}` |",
            "",
            "## Baseline vs Final",
            "",
            "| Field | Baseline | Final |",
            "|---|---|---|",
            f"| `headRefOid` | `{before['head']}` | `{after['head']}` |",
            f"| `mergeStateStatus` | `{before['merge']}` | `{after['merge']}` |",
            f"| `statusCheckRollup.state` | `{before['checks']}` | `{after['checks']}` |",
            f"| active unresolved review threads | `{before['unresolved']}` | `{after['unresolved']}` |",
            f"| changed files | `{len(before['changed_files'])}` | `{len(after['changed_files'])}` |",
            f"| head changed | `false` | `{str(head_changed).lower()}` |",
            "",
            "## Changed File Scope",
            "",
            "| Field | Value |",
            "|---|---|",
            f"| changed-file evidence | `{'collected' if after['files_collected'] else 'missing'}` |",
            f"| new changed paths | `{limited_path_list(new_changed_paths) or 'none'}` |",
            f"| removed changed paths | `{limited_path_list(removed_changed_paths) or 'none'}` |",
            "",
            "## Final Changed Files",
            "",
        ]
    )
    if after["changed_files"]:
        lines.extend(["| Path | Change | + | - |", "|---|---|---:|---:|"])
        for path, change_type, additions, deletions in file_rows(after["changed_files"]):
            lines.append(
                f"| `{markdown_cell(path)}` | `{markdown_cell(change_type)}` | `{additions}` | `{deletions}` |"
            )
    else:
        lines.append("- None collected")

    lines.extend(["", "## Blockers", ""])
    if snapshot_blockers:
        lines.extend([f"- {blocker}" for blocker in snapshot_blockers])
    else:
        lines.append("- None")

    lines.extend(["", "## Failed Hard Gates", ""])
    if failed_gates:
        lines.extend(["| Gate | Grade cap | Message |", "|---|---|---|"])
        for gate in failed_gates:
            lines.append(
                f"| `{markdown_cell(gate.get('name', 'unknown'))}` "
                f"| `{markdown_cell(gate.get('grade_cap') or 'none')}` "
                f"| {markdown_cell(gate.get('message', ''))} |"
            )
    else:
        lines.append("- None")

    return "\n".join(lines) + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--final", required=True, type=Path)
    parser.add_argument("--submission", required=True, type=Path)
    parser.add_argument("--task-detail", required=True, type=Path)
    parser.add_argument("--quality-snapshot", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--pr", required=True)
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--timed-out", action="store_true")
    parser.add_argument("--wait-secs", required=True)
    parser.add_argument("--max-rounds", required=True)
    parser.add_argument("--max-turns", required=True)
    parser.add_argument("--max-budget-usd", default="")
    parser.add_argument("--timeout-secs", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    args.output.write_text(render_report(args), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
