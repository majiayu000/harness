#!/usr/bin/env python3
"""Artifact helpers for PR repair live evaluations."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from evaluate_pr_repair_submit import write_json


JsonObject = dict[str, Any]


def read_json(path: Path, *, required: bool = True) -> Any:
    try:
        with path.open("r", encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        if required:
            raise SystemExit(f"failed to read JSON artifact {path}: {exc}") from exc
        return {}


def normalized_path(value: Any) -> str | None:
    if not isinstance(value, str) or not value.strip():
        return None
    return str(Path(value).expanduser().resolve(strict=False))


def project_registry_preflight(args: argparse.Namespace) -> int:
    project_root = str(Path(args.project_root).expanduser().resolve(strict=False))
    payload = read_json(args.projects_json)
    if isinstance(payload, list):
        projects = payload
    elif isinstance(payload, dict) and isinstance(payload.get("projects"), list):
        projects = payload["projects"]
    else:
        print("project registry preflight failed: /projects response is not a project list")
        return 2

    matches = [
        project
        for project in projects
        if isinstance(project, dict)
        and (
            normalized_path(project.get("root")) == project_root
            or normalized_path(project.get("id")) == project_root
        )
    ]
    active_matches = [project for project in matches if project.get("active", True)]
    if active_matches:
        return 0
    if matches:
        print(
            "project registry preflight failed: "
            f"project root is registered but inactive: {project_root}"
        )
    else:
        print(f"project registry preflight failed: project root is not registered: {project_root}")
    return 1


def write_preflight_failure(args: argparse.Namespace) -> int:
    submission = {
        "error": args.error,
        "eval_submission_mode": "prompt_task",
        "http_status": "preflight",
        "project_root": args.project_root,
        "server_url": args.server_url,
        "stage": "project_registry_preflight",
        "status": "failed",
    }
    task_detail = {
        "error": args.error,
        "project_root": args.project_root,
        "stage": "project_registry_preflight",
        "status": "failed",
    }
    write_json(args.submission, submission)
    write_json(args.task_detail, task_detail)
    return 0


def pr_facts(pr: JsonObject) -> JsonObject:
    threads = (((pr.get("reviewThreads") or {}).get("nodes")) or [])
    files = list(((pr.get("files") or {}).get("nodes")) or [])
    active_threads = [
        thread
        for thread in threads
        if not thread.get("isResolved", False) and not thread.get("isOutdated", False)
    ]
    return {
        "head": pr.get("headRefOid"),
        "merge": pr.get("mergeStateStatus") or "UNKNOWN",
        "checks": (pr.get("statusCheckRollup") or {}).get("state") or "UNKNOWN",
        "unresolved": len(active_threads),
        "files_collected": "files" in pr,
        "changed_files": files,
        "changed_file_paths": sorted(
            {
                item.get("path")
                for item in files
                if isinstance(item, dict) and item.get("path")
            }
        ),
    }


def candidate_class(facts: JsonObject) -> str:
    if facts["unresolved"] > 0:
        return "review_feedback_repair"
    if facts["checks"] != "SUCCESS":
        return "ci_repair"
    if facts["merge"] == "CLEAN" and facts["checks"] == "SUCCESS":
        return "ready_noop"
    return "mergeability_repair"


def snapshot_summary(pr: JsonObject) -> str:
    facts = pr_facts(pr)
    files = (((pr.get("files") or {}).get("nodes")) or [])
    return (
        f"pr={pr.get('number')} head={facts['head']} merge={facts['merge']} "
        f"checks={facts['checks']} unresolved_threads={facts['unresolved']} "
        f"changed_files={len(files)}"
    )


def markdown_cell(value: Any) -> str:
    return str(value).replace("|", "\\|")


def compact(value: Any, limit: int = 240) -> str:
    text = str(value).replace("\n", " ").strip()
    if len(text) <= limit:
        return text
    return f"{text[:limit - 3]}..."


def limited_path_list(paths: list[str]) -> str:
    if not paths:
        return ""
    shown = ", ".join(paths[:5])
    if len(paths) > 5:
        return f"{shown}, ... ({len(paths)} total)"
    return shown


def sorted_file_rows(files: list[JsonObject]) -> list[tuple[str, str, Any, Any]]:
    rows = []
    for item in sorted(files, key=lambda file: file.get("path") or ""):
        rows.append(
            (
                item.get("path") or "UNKNOWN",
                item.get("changeType") or "UNKNOWN",
                item.get("additions", 0),
                item.get("deletions", 0),
            )
        )
    return rows


def quality_fields(quality: JsonObject) -> tuple[Any, Any, Any, list[Any]]:
    blockers = quality.get("blocker_summary")
    if not isinstance(blockers, list):
        blockers = []
    return (
        quality.get("final_grade", "unknown"),
        quality.get("final_score", "unknown"),
        quality.get("grade_cap") or "none",
        blockers,
    )


def write_collect_report(args: argparse.Namespace) -> int:
    baseline = read_json(args.baseline)
    quality = read_json(args.quality)
    grade, score, grade_cap, blockers = quality_fields(quality)
    task_class = candidate_class(pr_facts(baseline))

    lines = [
        "# PR Repair Evaluation",
        "",
        f"- Run ID: `{args.run_id}`",
        f"- Repo: `{args.repo}`",
        f"- PR: `#{args.pr}`",
        "- Mode: collect-only",
        f"- Candidate class: `{task_class}`",
        f"- Grade: `{grade}`",
        f"- Score: `{score}`",
        f"- Grade cap: `{grade_cap}`",
        "",
        "## Baseline",
        "",
        "```text",
        snapshot_summary(baseline),
        "```",
        "",
        "## Blockers",
        "",
    ]
    lines.extend([f"- {blocker}" for blocker in blockers] or ["- None"])
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


def write_final_report(args: argparse.Namespace) -> int:
    baseline = read_json(args.baseline)
    final = read_json(args.final)
    submission = read_json(args.submission, required=False)
    task_detail = read_json(args.task_detail, required=False)
    quality = read_json(args.quality)

    before = pr_facts(baseline)
    after = pr_facts(final)
    head_changed = before["head"] != after["head"]
    new_changed_paths = sorted(set(after["changed_file_paths"]) - set(before["changed_file_paths"]))
    removed_changed_paths = sorted(
        set(before["changed_file_paths"]) - set(after["changed_file_paths"])
    )
    task_status = (
        task_detail.get("status")
        or (task_detail.get("workflow") or {}).get("state")
        or submission.get("status")
        or "unknown"
    )
    workflow_id = submission.get("workflow_id") or (task_detail.get("workflow") or {}).get("id")
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
    timed_out = args.timed_out == "1"
    grade, score, grade_cap, blockers = quality_fields(quality)

    lines = [
        "# PR Repair Evaluation",
        "",
        f"- Run ID: `{args.run_id}`",
        f"- Repo: `{args.repo}`",
        f"- PR: `#{args.pr}`",
        f"- Server URL: `{args.server_url}`",
        f"- Candidate class: `{candidate_class(before)}`",
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
            f"- Timed out: `{str(timed_out).lower()}`",
            f"- Grade: `{grade}`",
            f"- Score: `{score}`",
            f"- Grade cap: `{grade_cap}`",
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
        for path, change_type, additions, deletions in sorted_file_rows(after["changed_files"]):
            lines.append(
                f"| `{markdown_cell(path)}` | `{markdown_cell(change_type)}` | "
                f"`{additions}` | `{deletions}` |"
            )
    else:
        lines.append("- None collected")

    lines.extend(["", "## Blockers", ""])
    lines.extend([f"- {blocker}" for blocker in blockers] or ["- None"])
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    preflight = sub.add_parser("preflight-project-registry")
    preflight.add_argument("--project-root", required=True)
    preflight.add_argument("--projects-json", type=Path, required=True)
    preflight.set_defaults(func=project_registry_preflight)

    failure = sub.add_parser("write-preflight-failure")
    failure.add_argument("--submission", type=Path, required=True)
    failure.add_argument("--task-detail", type=Path, required=True)
    failure.add_argument("--project-root", required=True)
    failure.add_argument("--server-url", required=True)
    failure.add_argument("--error", required=True)
    failure.set_defaults(func=write_preflight_failure)

    collect = sub.add_parser("write-collect-report")
    collect.add_argument("--baseline", type=Path, required=True)
    collect.add_argument("--quality", type=Path, required=True)
    collect.add_argument("--output", type=Path, required=True)
    collect.add_argument("--run-id", required=True)
    collect.add_argument("--repo", required=True)
    collect.add_argument("--pr", required=True)
    collect.set_defaults(func=write_collect_report)

    final = sub.add_parser("write-final-report")
    final.add_argument("--baseline", type=Path, required=True)
    final.add_argument("--final", type=Path, required=True)
    final.add_argument("--submission", type=Path, required=True)
    final.add_argument("--task-detail", type=Path, required=True)
    final.add_argument("--quality", type=Path, required=True)
    final.add_argument("--output", type=Path, required=True)
    final.add_argument("--run-id", required=True)
    final.add_argument("--repo", required=True)
    final.add_argument("--pr", required=True)
    final.add_argument("--server-url", required=True)
    final.add_argument("--timed-out", required=True)
    final.add_argument("--wait-secs", required=True)
    final.add_argument("--max-rounds", required=True)
    final.add_argument("--max-turns", required=True)
    final.add_argument("--max-budget-usd", default="")
    final.add_argument("--timeout-secs", required=True)
    final.set_defaults(func=write_final_report)

    return parser


def main() -> int:
    args = build_parser().parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
