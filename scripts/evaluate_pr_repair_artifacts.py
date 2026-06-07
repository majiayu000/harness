#!/usr/bin/env python3
"""Artifact helpers for PR repair live evaluations."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from evaluate_pr_repair_submit import write_json


JsonObject = dict[str, Any]

TERMINAL_TASK_STATES = {
    "blocked",
    "cancelled",
    "canceled",
    "done",
    "expired",
    "failed",
    "passed",
    "ready_to_merge",
    "success",
    "succeeded",
}

ACTIVE_SCHEDULER_STATES = {
    "awaiting_dependencies",
    "dispatching",
    "pending",
    "queued",
    "running",
}


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
    stage = args.stage or "project_registry_preflight"
    submission = {
        "error": args.error,
        "eval_submission_mode": "prompt_task",
        "http_status": "preflight",
        "project_root": args.project_root,
        "server_url": args.server_url,
        "stage": stage,
        "status": "failed",
    }
    task_detail = {
        "error": args.error,
        "project_root": args.project_root,
        "stage": stage,
        "status": "failed",
    }
    write_json(str(args.submission), submission)
    write_json(str(args.task_detail), task_detail)
    return 0


def normalized_state(value: Any) -> str:
    if not isinstance(value, str):
        return ""
    return value.strip().lower()


def terminal_state_from_task(task: JsonObject) -> str:
    workflow = task.get("workflow")
    workflow_state = ""
    if isinstance(workflow, dict):
        workflow_state = normalized_state(workflow.get("state") or workflow.get("status"))
    candidates = [
        workflow_state,
        normalized_state(task.get("workflow_state")),
        normalized_state(task.get("workflowStatus")),
        normalized_state(task.get("status")),
        normalized_state(task.get("state")),
    ]
    for candidate in candidates:
        if candidate in TERMINAL_TASK_STATES:
            return candidate
    if normalized_state(task.get("phase")) == "terminal":
        return "terminal"
    return ""


def task_is_active(task: JsonObject) -> bool:
    if terminal_state_from_task(task):
        return False

    scheduler = task.get("scheduler")
    scheduler_state = ""
    if isinstance(scheduler, dict):
        scheduler_state = normalized_state(scheduler.get("authority_state"))
    if scheduler_state in ACTIVE_SCHEDULER_STATES:
        return True
    if scheduler_state in TERMINAL_TASK_STATES:
        return False

    phase = normalized_state(task.get("phase"))
    return phase != "terminal"


def conflicting_eval_tasks(
    tasks_response: Any,
    *,
    external_id: str,
    target_prefix: str | None = None,
    task_id: str | None = None,
    workflow_id: str | None = None,
) -> list[JsonObject]:
    tasks = tasks_response.get("data") if isinstance(tasks_response, dict) else None
    if not isinstance(tasks, list):
        raise SystemExit("Harness task list response is missing a data array")

    conflicts = []
    for item in tasks:
        if not isinstance(item, dict):
            continue
        item_external_id = item.get("external_id")
        target_matches = item_external_id == external_id
        if target_prefix:
            target_matches = target_matches or item_external_id == target_prefix
            target_matches = target_matches or (
                isinstance(item_external_id, str)
                and item_external_id.startswith(f"{target_prefix}:")
            )
        if not target_matches:
            continue
        if task_id and item.get("id") == task_id:
            continue
        if workflow_id and item.get("workflow_id") == workflow_id:
            continue
        if task_is_active(item):
            conflicts.append(item)
    return conflicts


def conflict_message(conflicts: list[JsonObject], project_root: str, external_id: str) -> str:
    rows = []
    for task in conflicts[:8]:
        scheduler = task.get("scheduler")
        scheduler_state = (
            scheduler.get("authority_state") if isinstance(scheduler, dict) else "<unknown>"
        )
        rows.append(
            "task_id={task_id} project={project} status={status} scheduler={scheduler} workflow={workflow}".format(
                task_id=task.get("id") or task.get("task_id") or "<unknown>",
                project=task.get("project") or "<unknown>",
                status=task.get("status") or "<unknown>",
                scheduler=scheduler_state,
                workflow=task.get("workflow_id") or "<unknown>",
            )
        )
    suffix = "; ".join(rows)
    return (
        f"Conflicting active PR repair eval already exists for {external_id}; "
        f"target project={project_root}; conflicts: {suffix}"
    )


def check_conflicts(args: argparse.Namespace) -> int:
    conflicts = conflicting_eval_tasks(
        read_json(args.tasks_json),
        external_id=args.external_id,
        target_prefix=args.target_prefix or None,
    )
    if conflicts:
        print(conflict_message(conflicts, args.project_root, args.external_id))
        return 1
    return 0


def pr_facts(pr: JsonObject) -> JsonObject:
    review_threads = pr.get("reviewThreads")
    thread_nodes = review_threads.get("nodes") if isinstance(review_threads, dict) else None
    threads = [thread for thread in (thread_nodes or []) if isinstance(thread, dict)]
    files_container = pr.get("files")
    file_nodes = files_container.get("nodes") if isinstance(files_container, dict) else None
    files = [item for item in (file_nodes or []) if isinstance(item, dict)]
    status_check_rollup = pr.get("statusCheckRollup")
    active_threads = [
        thread
        for thread in threads
        if not thread.get("isResolved", False)
        and not thread.get("isOutdated", False)
    ]
    return {
        "head": pr.get("headRefOid"),
        "merge": pr.get("mergeStateStatus") or "UNKNOWN",
        "checks": (
            status_check_rollup.get("state") if isinstance(status_check_rollup, dict) else None
        )
        or "UNKNOWN",
        "unresolved": len(active_threads),
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
    if not isinstance(baseline, dict):
        baseline = {}
    quality = read_json(args.quality)
    if not isinstance(quality, dict):
        quality = {}
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
    if not isinstance(baseline, dict):
        baseline = {}
    final = read_json(args.final)
    if not isinstance(final, dict):
        final = {}
    submission = read_json(args.submission, required=False)
    if not isinstance(submission, dict):
        submission = {}
    task_detail = read_json(args.task_detail, required=False)
    if not isinstance(task_detail, dict):
        task_detail = {}
    quality = read_json(args.quality)
    if not isinstance(quality, dict):
        quality = {}

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


def iter_workflow_nodes(node: Any):
    if not isinstance(node, dict):
        return
    yield node
    children = node.get("children")
    if isinstance(children, list):
        for child in children:
            yield from iter_workflow_nodes(child)


def find_workflow_node(
    runtime_tree: Any,
    workflow_id: str | None,
    task_id: str | None,
) -> JsonObject | None:
    workflows = runtime_tree.get("workflows") if isinstance(runtime_tree, dict) else None
    if not isinstance(workflows, list):
        return None
    for root in workflows:
        for node in iter_workflow_nodes(root):
            workflow = node.get("workflow")
            if not isinstance(workflow, dict):
                continue
            workflow_data = workflow.get("data") if isinstance(workflow.get("data"), dict) else {}
            task_ids = (
                workflow_data.get("task_ids")
                if isinstance(workflow_data.get("task_ids"), list)
                else []
            )
            if workflow_id and workflow.get("id") == workflow_id:
                return node
            if task_id and (
                workflow_data.get("task_id") == task_id
                or workflow_data.get("submission_id") == task_id
                or task_id in task_ids
            ):
                return node
    return None


def artifact_count(job: JsonObject) -> int:
    output = job.get("output")
    if isinstance(output, dict):
        artifacts = output.get("artifacts")
        if isinstance(artifacts, list):
            return len(artifacts)
    return 0


def activity_from_job(job: JsonObject) -> str | None:
    output = job.get("output")
    if isinstance(output, dict) and isinstance(output.get("activity"), str):
        return output["activity"]
    input_value = job.get("input")
    if isinstance(input_value, dict) and isinstance(input_value.get("activity"), str):
        return input_value["activity"]
    return None


def error_kind_from_job(job: JsonObject) -> str | None:
    for key in ("error_kind", "errorKind"):
        value = job.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    output = job.get("output")
    if isinstance(output, dict):
        value = output.get("error_kind") or output.get("errorKind")
        if isinstance(value, str) and value.strip():
            return value.strip()
    envelope = job.get("activity_result_envelope")
    final_result = envelope.get("final_result") if isinstance(envelope, dict) else None
    if isinstance(final_result, dict):
        value = final_result.get("error_kind") or final_result.get("errorKind")
        if isinstance(value, str) and value.strip():
            return value.strip()
    return None


def runtime_jobs_from_node(node: JsonObject) -> tuple[list[JsonObject], str | None]:
    jobs = []
    latest_activity = None
    commands = node.get("commands")
    if not isinstance(commands, list):
        return jobs, latest_activity
    for command in commands:
        if not isinstance(command, dict):
            continue
        runtime_jobs = command.get("runtime_jobs")
        if not isinstance(runtime_jobs, list):
            continue
        for job in runtime_jobs:
            if not isinstance(job, dict):
                continue
            activity = activity_from_job(job)
            latest_activity = activity or latest_activity
            status = job.get("status") if isinstance(job.get("status"), str) else ""
            jobs.append(
                {
                    "activity": activity,
                    "artifact_count": artifact_count(job),
                    "error_kind": error_kind_from_job(job),
                    "id": job.get("id") or "",
                    "runtime_job_id": job.get("id") or "",
                    "state": status,
                    "status": status,
                    "terminal_state": (
                        status if normalized_state(status) in TERMINAL_TASK_STATES else None
                    ),
                }
            )
    return jobs, latest_activity


def merge_runtime_tree_data(
    task_detail: JsonObject,
    runtime_tree: Any,
    *,
    workflow_id: str | None,
    task_id: str | None,
) -> JsonObject:
    node = find_workflow_node(runtime_tree, workflow_id, task_id)
    artifact = {
        "source": "workflow_runtime_tree",
        "workflow_id": workflow_id,
        "task_id": task_id,
        "status": "workflow_not_found",
        "runtime_job_count": 0,
    }
    if node is None:
        task_detail["runtime_tree_artifact"] = artifact
        return task_detail

    workflow = node.get("workflow") if isinstance(node.get("workflow"), dict) else {}
    jobs, latest_activity = runtime_jobs_from_node(node)
    artifact["status"] = "matched"
    artifact["runtime_job_count"] = len(jobs)
    task_detail["runtime_tree_artifact"] = artifact
    task_detail["runtime_jobs"] = jobs
    if latest_activity and not task_detail.get("latest_activity"):
        task_detail["latest_activity"] = latest_activity

    detail_workflow = task_detail.get("workflow")
    if not isinstance(detail_workflow, dict):
        detail_workflow = {}
    if workflow.get("id") and not detail_workflow.get("id"):
        detail_workflow["id"] = workflow.get("id")
    if workflow.get("state") and not detail_workflow.get("state"):
        detail_workflow["state"] = workflow.get("state")
    if workflow.get("project_id") and not detail_workflow.get("project_id"):
        detail_workflow["project_id"] = workflow.get("project_id")
    if detail_workflow:
        task_detail["workflow"] = detail_workflow
    return task_detail


def merge_runtime_tree(args: argparse.Namespace) -> int:
    raw_task_detail = read_json(args.task_detail, required=False)
    task_detail = raw_task_detail if isinstance(raw_task_detail, dict) else {}
    runtime_tree = read_json(args.runtime_tree)
    merged = merge_runtime_tree_data(
        task_detail,
        runtime_tree,
        workflow_id=args.workflow_id or None,
        task_id=args.task_id or None,
    )
    write_json(str(args.task_detail), merged)
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
    failure.add_argument("--stage", default="project_registry_preflight")
    failure.set_defaults(func=write_preflight_failure)

    conflicts = sub.add_parser("check-conflicts")
    conflicts.add_argument("--tasks-json", required=True, type=Path)
    conflicts.add_argument("--project-root", required=True)
    conflicts.add_argument("--external-id", required=True)
    conflicts.add_argument("--target-prefix", default="")
    conflicts.set_defaults(func=check_conflicts)

    runtime_tree = sub.add_parser("merge-runtime-tree")
    runtime_tree.add_argument("--task-detail", required=True, type=Path)
    runtime_tree.add_argument("--runtime-tree", required=True, type=Path)
    runtime_tree.add_argument("--workflow-id", default="")
    runtime_tree.add_argument("--task-id", default="")
    runtime_tree.set_defaults(func=merge_runtime_tree)

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
