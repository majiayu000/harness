#!/usr/bin/env python3
"""Preflight helpers for PR repair eval runs."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any


def load_projects(path: Path) -> list[dict[str, Any]]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"failed to read Harness project registry: {exc}") from exc
    if not isinstance(data, list):
        raise ValueError("Harness project registry response is not a project list")
    return [project for project in data if isinstance(project, dict)]


def find_registered_project(raw_project: str, projects: list[dict[str, Any]]) -> str:
    project_exists = os.path.isdir(raw_project)
    canonical_project = os.path.realpath(raw_project) if project_exists else None
    inactive_matches: list[str] = []
    registered: list[str] = []

    for project in projects:
        project_id = project.get("id")
        project_name = project.get("name")
        root = project.get("root")
        if isinstance(root, str) and root:
            registered.append(root)

        root_matches = False
        if isinstance(root, str) and root:
            root_matches = raw_project == root
            if canonical_project is not None:
                root_matches = root_matches or os.path.realpath(root) == canonical_project

        id_matches = isinstance(project_id, str) and raw_project == project_id
        name_matches = isinstance(project_name, str) and raw_project == project_name
        if not (root_matches or id_matches or name_matches):
            continue

        display = str(root or project_id or project_name or raw_project)
        if project.get("active") is False:
            inactive_matches.append(display)
            continue
        if isinstance(root, str) and root:
            return root
        raise ValueError(f"matched project has no usable root: {display}")

    if inactive_matches:
        raise ValueError("matched project is inactive: " + ", ".join(sorted(inactive_matches)))

    preview = ", ".join(sorted(set(registered))[:8])
    suffix = f"; registered roots: {preview}" if preview else "; no registered roots returned"
    raise ValueError(f"Harness project root is not registered: {raw_project}{suffix}")


def write_preflight_failure(submission_path: Path, task_detail_path: Path, reason: str) -> None:
    submission = {
        "error": reason,
        "eval_submission_mode": "prompt_task",
        "http_status": "000",
        "preflight": "server_project_registry",
        "status": "failed",
    }
    task_detail = {
        "error": reason,
        "preflight": "server_project_registry",
        "status": "failed",
    }
    for path, payload in ((submission_path, submission), (task_detail_path, task_detail)):
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    check = subparsers.add_parser("check-project")
    check.add_argument("--project-root", required=True)
    check.add_argument("--projects-json", required=True, type=Path)

    failure = subparsers.add_parser("write-failure")
    failure.add_argument("--submission", required=True, type=Path)
    failure.add_argument("--task-detail", required=True, type=Path)
    failure.add_argument("--reason", required=True)

    args = parser.parse_args()
    if args.command == "check-project":
        try:
            print(find_registered_project(args.project_root, load_projects(args.projects_json)))
        except ValueError as exc:
            print(str(exc), file=sys.stderr)
            return 1
        return 0

    if args.command == "write-failure":
        write_preflight_failure(args.submission, args.task_detail, args.reason)
        return 0

    parser.error(f"unknown command: {args.command}")
    return 2


if __name__ == "__main__":
    sys.exit(main())
