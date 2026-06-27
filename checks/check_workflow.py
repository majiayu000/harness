#!/usr/bin/env python3
"""Validate a SpecRail workflow pack without network or GitHub writes."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

from specrail_lib import (
    SpecRailError,
    load_pack,
    read_text,
    validate_action_policy,
    validate_json_schemas,
    validate_labels,
    validate_state_graph,
    validate_template_parity,
)


REQUIRED_FILES = [
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "SPEC.md",
    "docs/ADOPTION_MATRIX.md",
    "workflow.yaml",
    "states.yaml",
    "labels.yaml",
    "examples/adoptions/matrix.json",
    "checks/github_pr_evidence.py",
    "checks/pr_gate.py",
    "checks/route_gate.py",
    "templates/issue_bug.md",
    "templates/issue_feature.md",
    "templates/product_spec.md",
    "templates/tech_spec.md",
    "templates/tasks.md",
    "templates/pull_request.md",
    "review/agent_first_review.md",
    "review/human_final_review.md",
    "policies/security_disclosure.md",
    "policies/maintainer_escalation.md",
    "schemas/flow_manifest.schema.json",
    "schemas/issue_triage.schema.json",
    "schemas/evaluation_result.schema.json",
    "schemas/adoption_matrix.schema.json",
    "schemas/spec_packet.schema.json",
    "schemas/task_plan.schema.json",
    "schemas/pr_review_gate.schema.json",
    "schemas/workflow_run.schema.json",
]

REQUIRED_TOKENS = {
    "workflow.yaml": [
        "default_mode: dry_run",
        "forbidden_agent_actions:",
        "required_human_gates:",
        "action_policy:",
    ],
    "states.yaml": [
        "ready_to_spec",
        "ready_to_implement",
        "agent_review",
        "human_review",
        "merge_ready",
    ],
    "labels.yaml": [
        "readiness:",
        "ready_to_spec",
        "ready_to_implement",
        "security_private",
    ],
    "templates/product_spec.md": [
        "## Goals",
        "## Non-Goals",
        "## Acceptance Criteria",
    ],
    "templates/tech_spec.md": [
        "## Proposed Design",
        "## Test Plan",
        "## Rollback Plan",
    ],
    "templates/tasks.md": [
        "## Implementation Tasks",
        "## Verification",
        "## Handoff Notes",
    ],
    "templates/pull_request.md": [
        "## Linked Work",
        "## Readiness Gate",
        "## Review Gate",
        "## Merge Gate",
        "## Verification",
    ],
}


def validate_required_files(repo: Path) -> list[str]:
    errors: list[str] = []
    for rel in REQUIRED_FILES:
        path = repo / rel
        if not path.is_file():
            errors.append(f"missing required file: {rel}")
    return errors


def validate_tokens(repo: Path) -> list[str]:
    errors: list[str] = []
    for rel, tokens in REQUIRED_TOKENS.items():
        path = repo / rel
        if not path.is_file():
            continue
        text = read_text(path)
        for token in tokens:
            if token not in text:
                errors.append(f"{rel}: missing token {token!r}")
    return errors


def validate_spec_packet(spec_dir: Path) -> list[str]:
    errors: list[str] = []
    if not spec_dir.exists():
        return [f"spec packet does not exist: {spec_dir}"]
    if not spec_dir.is_dir():
        return [f"spec packet is not a directory: {spec_dir}"]

    match = re.fullmatch(r"GH([0-9]+)", spec_dir.name)
    if not match:
        errors.append(f"{spec_dir}: spec packet directory must be named GH<number>")
        issue_number = None
    else:
        issue_number = match.group(1)

    issue_tokens = []
    if issue_number:
        issue_tokens = [f"GH-{issue_number}", f"GH{issue_number}", f"#{issue_number}"]

    for name in ["product.md", "tech.md"]:
        path = spec_dir / name
        if not path.is_file():
            errors.append(f"{spec_dir}: missing {name}")
            continue
        text = read_text(path)
        if not text.strip():
            errors.append(f"{path}: must not be empty")
        if issue_tokens and not any(token in text for token in issue_tokens):
            errors.append(f"{path}: missing linked issue token {' or '.join(issue_tokens)}")

    task_path = spec_dir / "tasks.md"
    if task_path.is_file():
        errors.extend(validate_task_plan(task_path, issue_number))
    return errors


def validate_task_plan(path: Path, issue_number: str | None) -> list[str]:
    errors: list[str] = []
    text = read_text(path)
    if not text.strip():
        return [f"{path}: must not be empty"]
    task_id_pattern = (
        re.compile(rf"SP{re.escape(issue_number)}-T[0-9]+")
        if issue_number
        else re.compile(r"SP[0-9]+-T[0-9]+")
    )
    expected_task_id = f"SP{issue_number}-T[0-9]+" if issue_number else "SP<number>-T[0-9]+"
    ids: list[str] = []
    for line_number, line in implementation_task_lines(text):
        if "- [" not in line:
            continue
        match = re.search(r"`([^`]+)`", line)
        if not match:
            errors.append(f"{path}:{line_number}: task is missing stable ID")
            continue
        task_id = match.group(1)
        ids.append(task_id)
        if not task_id_pattern.fullmatch(task_id):
            errors.append(f"{path}:{line_number}: task ID {task_id} must match {expected_task_id}")
        for token in ["Owner:", "Done when:", "Verify:"]:
            if token not in line:
                errors.append(f"{path}:{line_number}: task {task_id} missing {token}")
    if not ids:
        errors.append(f"{path}: no task checklist items found")
    duplicates = sorted({task_id for task_id in ids if ids.count(task_id) > 1})
    for duplicate in duplicates:
        errors.append(f"{path}: duplicate task ID {duplicate}")
    return errors


def implementation_task_lines(text: str) -> list[tuple[int, str]]:
    lines = text.splitlines()
    has_section = any(line.strip() == "## Implementation Tasks" for line in lines)
    in_tasks = not has_section
    task_lines: list[tuple[int, str]] = []
    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if has_section and stripped.startswith("## "):
            in_tasks = stripped == "## Implementation Tasks"
            continue
        if in_tasks:
            task_lines.append((line_number, line))
    return task_lines


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate a SpecRail workflow pack."
    )
    parser.add_argument("--repo", default=".", help="Workflow pack root")
    parser.add_argument(
        "--spec-dir",
        action="append",
        default=[],
        help="Optional specs/GH<number> directory to validate",
    )
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    errors: list[str] = []
    try:
        config = load_pack(repo)
        errors.extend(validate_required_files(repo))
        errors.extend(validate_tokens(repo))
        errors.extend(validate_json_schemas(repo))
        errors.extend(validate_state_graph(config))
        errors.extend(validate_labels(config))
        errors.extend(validate_action_policy(config))
        errors.extend(validate_template_parity(repo))
        for raw_spec_dir in args.spec_dir:
            errors.extend(validate_spec_packet((repo / raw_spec_dir).resolve()))
    except SpecRailError as exc:
        errors.append(str(exc))

    if errors:
        print("SpecRail check failed")
        for error in errors:
            print(f"- {error}")
        return 1

    print("SpecRail check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
