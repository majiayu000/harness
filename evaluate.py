#!/usr/bin/env python3
"""Evaluate a SpecRail spec packet and adoption smoke evidence."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


CHECKS_DIR = Path(__file__).resolve().parent / "checks"
sys.path.insert(0, str(CHECKS_DIR))

from specrail_lib import SpecRailError, load_pack, read_text  # noqa: E402


REQUIRED_SMOKE_IDS = [
    "rclean.new_rule_spec_first",
    "rclean.security_boundary_gate",
    "rclean.doc_only_direct",
    "rclean.ci_command_mapping",
    "rclean.issue_dedupe",
]

REQUIRED_RCLEAN_COMMANDS = [
    "cargo fmt -- --check",
    "cargo clippy --all-targets --all-features -- -D warnings",
    "cargo test",
    "cargo build --release",
    "rustup run 1.95 cargo build",
    "rustup run 1.95 cargo test",
]

ADOPTION_MATRIX_DOC = "docs/ADOPTION_MATRIX.md"
ADOPTION_MATRIX_FIXTURE = "examples/adoptions/matrix.json"
REQUIRED_ADOPTION_IDS = [
    "rclean",
    "litellm-rs",
    "claude-code-monitor",
]
ADOPTION_LEVELS = {
    "referenced",
    "smoke",
    "spec_packet",
    "pr_gate",
    "repo_integrated",
    "automation_ready",
}
ADOPTION_STATUSES = {"active", "needs_human", "reference_only", "blocked"}


def check(status: str, check_id: str, path: str, message: str) -> dict[str, str]:
    return {"id": check_id, "status": status, "path": path, "message": message}


def repo_relative_file(repo: Path, raw_path: str) -> tuple[Path | None, str | None]:
    path = Path(raw_path)
    if path.is_absolute():
        return None, "path must be relative to the SpecRail repo"
    if ".." in path.parts:
        return None, "path must not contain '..'"
    resolved_repo = repo.resolve()
    resolved_path = (resolved_repo / path).resolve()
    try:
        resolved_path.relative_to(resolved_repo)
    except ValueError:
        return None, "path must stay inside the SpecRail repo"
    return resolved_path, None


def spec_issue_number(spec_dir: Path) -> str | None:
    match = re.fullmatch(r"GH([0-9]+)", spec_dir.name)
    return match.group(1) if match else None


def issue_tokens(issue_number: str | None) -> list[str]:
    if not issue_number:
        return []
    return [f"GH-{issue_number}", f"GH{issue_number}", f"#{issue_number}"]


def evaluate_spec(repo: Path, spec_dir: Path) -> tuple[list[dict[str, str]], list[str]]:
    checks: list[dict[str, str]] = []
    errors: list[str] = []
    issue_number = spec_issue_number(spec_dir)

    for name, check_id in [
        ("product.md", "spec.product_present"),
        ("tech.md", "spec.tech_present"),
    ]:
        path = spec_dir / name
        rel = str(path.relative_to(repo))
        if path.is_file() and read_text(path).strip():
            checks.append(check("pass", check_id, rel, f"{name} exists and is non-empty"))
        else:
            checks.append(check("fail", check_id, rel, f"{name} is missing or empty"))
            errors.append(f"{rel} is missing or empty")

    task_path = spec_dir / "tasks.md"
    task_rel = str(task_path.relative_to(repo))
    if task_path.is_file() and read_text(task_path).strip():
        checks.append(check("pass", "spec.tasks_present", task_rel, "tasks.md exists and is non-empty"))
    elif task_path.is_file():
        checks.append(check("fail", "spec.tasks_present", task_rel, "tasks.md is empty"))
        errors.append(f"{task_rel} is empty")
    else:
        checks.append(
            check(
                "pass",
                "spec.tasks_optional",
                task_rel,
                "tasks.md may be created later by the implement route",
            )
        )

    for name in ["product.md", "tech.md"]:
        path = spec_dir / name
        rel = str(path.relative_to(repo))
        if not path.is_file():
            continue
        text = read_text(path)
        if any(token in text for token in issue_tokens(issue_number)):
            checks.append(check("pass", "spec.issue_anchor_present", rel, "issue anchor present"))
        else:
            checks.append(check("fail", "spec.issue_anchor_present", rel, "issue anchor missing"))
            errors.append(f"{rel} is missing issue anchor")

    if task_path.is_file():
        task_errors = validate_tasks(repo, task_path, issue_number)
        if task_errors:
            for error in task_errors:
                checks.append(check("fail", "tasks.format", str(task_path.relative_to(repo)), error))
            errors.extend(task_errors)
        else:
            rel = str(task_path.relative_to(repo))
            checks.append(check("pass", "tasks.ids_unique", rel, "task IDs are unique"))
            checks.append(check("pass", "tasks.done_when_present", rel, "all tasks include Done when"))
            checks.append(check("pass", "tasks.verification_present", rel, "all tasks include Verify"))
    return checks, errors


def validate_tasks(repo: Path, task_path: Path, issue_number: str | None) -> list[str]:
    errors: list[str] = []
    text = read_text(task_path)
    prefix = f"SP{issue_number}-T" if issue_number else "SP"
    ids: list[str] = []
    for line_number, line in implementation_task_lines(text):
        if "- [" not in line:
            continue
        match = re.search(r"`([^`]+)`", line)
        if not match:
            errors.append(f"{task_path.relative_to(repo)}:{line_number}: missing task ID")
            continue
        task_id = match.group(1)
        ids.append(task_id)
        if issue_number and not task_id.startswith(prefix):
            errors.append(f"{task_path.relative_to(repo)}:{line_number}: {task_id} must start with {prefix}")
        for token in ["Owner:", "Done when:", "Verify:"]:
            if token not in line:
                errors.append(f"{task_path.relative_to(repo)}:{line_number}: {task_id} missing {token}")
    if not ids:
        errors.append(f"{task_path.relative_to(repo)}: no tasks found")
    for task_id in sorted({task_id for task_id in ids if ids.count(task_id) > 1}):
        errors.append(f"{task_path.relative_to(repo)}: duplicate task ID {task_id}")
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


def evaluate_rclean_smoke(repo: Path) -> tuple[list[dict[str, str]], list[str], list[str]]:
    smoke = repo / "examples" / "rclean-smoke.md"
    rel = "examples/rclean-smoke.md"
    checks: list[dict[str, str]] = []
    errors: list[str] = []
    warnings: list[str] = []
    if not smoke.is_file() or not read_text(smoke).strip():
        checks.append(check("fail", "rclean_smoke.present", rel, "rclean smoke file missing"))
        errors.append(f"{rel} is missing")
        return checks, errors, warnings

    text = read_text(smoke)
    checks.append(check("pass", "rclean_smoke.present", rel, "rclean smoke file exists"))
    if "read-only" in text and "Do not modify" in text:
        checks.append(check("pass", "rclean_smoke.read_only", rel, "read-only boundary recorded"))
    else:
        checks.append(check("fail", "rclean_smoke.read_only", rel, "read-only boundary missing"))
        errors.append("rclean smoke is missing read-only boundary")

    missing_ids = [scenario for scenario in REQUIRED_SMOKE_IDS if scenario not in text]
    if missing_ids:
        checks.append(check("fail", "rclean_smoke.scenarios_present", rel, f"missing scenarios: {', '.join(missing_ids)}"))
        errors.append(f"rclean smoke missing scenarios: {', '.join(missing_ids)}")
    else:
        checks.append(check("pass", "rclean_smoke.scenarios_present", rel, "all required scenarios recorded"))

    missing_commands = [command for command in REQUIRED_RCLEAN_COMMANDS if command not in text]
    if missing_commands:
        checks.append(check("fail", "rclean_smoke.ci_commands_present", rel, f"missing commands: {', '.join(missing_commands)}"))
        errors.append(f"rclean smoke missing CI commands: {', '.join(missing_commands)}")
    else:
        checks.append(check("pass", "rclean_smoke.ci_commands_present", rel, "all rclean CI commands recorded"))

    if "NOT SUBMITTED YET" in text and "drafts/rclean-issues-draft-2026-05-25.md" in text:
        checks.append(check("needs_human", "rclean_smoke.issue_dedupe_present", rel, "local draft issue must be reviewed before new issue creation"))
        warnings.append("rclean adoption smoke requires human review before creating duplicate issues")
    else:
        checks.append(check("fail", "rclean_smoke.issue_dedupe_present", rel, "draft issue dedupe evidence missing"))
        errors.append("rclean smoke missing issue dedupe evidence")
    return checks, errors, warnings


def evaluate_adoption_matrix(repo: Path) -> tuple[list[dict[str, str]], list[str], list[str]]:
    checks: list[dict[str, str]] = []
    errors: list[str] = []
    warnings: list[str] = []

    doc = repo / ADOPTION_MATRIX_DOC
    if doc.is_file() and read_text(doc).strip():
        checks.append(check("pass", "adoption_matrix.doc_present", ADOPTION_MATRIX_DOC, "adoption matrix doc exists"))
    else:
        checks.append(check("fail", "adoption_matrix.doc_present", ADOPTION_MATRIX_DOC, "adoption matrix doc missing"))
        errors.append(f"{ADOPTION_MATRIX_DOC} is missing")

    fixture = repo / ADOPTION_MATRIX_FIXTURE
    if not fixture.is_file():
        checks.append(check("fail", "adoption_matrix.fixture_present", ADOPTION_MATRIX_FIXTURE, "adoption matrix fixture missing"))
        errors.append(f"{ADOPTION_MATRIX_FIXTURE} is missing")
        return checks, errors, warnings

    try:
        payload = json.loads(read_text(fixture))
    except json.JSONDecodeError as exc:
        checks.append(check("fail", "adoption_matrix.fixture_json", ADOPTION_MATRIX_FIXTURE, f"invalid JSON: {exc.msg}"))
        errors.append(f"{ADOPTION_MATRIX_FIXTURE} invalid JSON: {exc.msg}")
        return checks, errors, warnings

    if not isinstance(payload, dict):
        checks.append(check("fail", "adoption_matrix.fixture_json", ADOPTION_MATRIX_FIXTURE, "JSON root must be an object"))
        errors.append(f"{ADOPTION_MATRIX_FIXTURE} JSON root must be an object")
        return checks, errors, warnings

    checks.append(check("pass", "adoption_matrix.fixture_present", ADOPTION_MATRIX_FIXTURE, "adoption matrix fixture exists"))
    adoptions = payload.get("adoptions")
    if not isinstance(adoptions, list):
        checks.append(check("fail", "adoption_matrix.records_shape", ADOPTION_MATRIX_FIXTURE, "adoptions must be a list"))
        errors.append("adoption matrix adoptions must be a list")
        return checks, errors, warnings

    by_id = {str(item.get("id")): item for item in adoptions if isinstance(item, dict) and item.get("id")}
    missing_ids = [adoption_id for adoption_id in REQUIRED_ADOPTION_IDS if adoption_id not in by_id]
    if missing_ids:
        checks.append(check("fail", "adoption_matrix.required_ids", ADOPTION_MATRIX_FIXTURE, f"missing IDs: {', '.join(missing_ids)}"))
        errors.append(f"adoption matrix missing IDs: {', '.join(missing_ids)}")
    else:
        checks.append(check("pass", "adoption_matrix.required_ids", ADOPTION_MATRIX_FIXTURE, "all required adoption IDs present"))

    for adoption_id in REQUIRED_ADOPTION_IDS:
        entry = by_id.get(adoption_id)
        if not isinstance(entry, dict):
            continue
        entry_path = f"{ADOPTION_MATRIX_FIXTURE}#{adoption_id}"
        level = entry.get("current_level")
        status = entry.get("status")
        evidence = entry.get("evidence")
        verified_behaviors = entry.get("verified_behaviors")
        next_gap = entry.get("next_gap")

        if level in ADOPTION_LEVELS:
            checks.append(check("pass", "adoption_matrix.level_valid", entry_path, f"{adoption_id} level is {level}"))
        else:
            checks.append(check("fail", "adoption_matrix.level_valid", entry_path, f"{adoption_id} has invalid level {level!r}"))
            errors.append(f"{adoption_id} has invalid adoption level")

        if status in ADOPTION_STATUSES:
            checks.append(check("pass", "adoption_matrix.status_valid", entry_path, f"{adoption_id} status is {status}"))
            if status == "needs_human":
                checks.append(
                    check(
                        "needs_human",
                        "adoption_matrix.status_needs_human",
                        entry_path,
                        f"{adoption_id} adoption still needs human review",
                    )
                )
                warnings.append(f"{adoption_id} adoption still needs human review")
        else:
            checks.append(check("fail", "adoption_matrix.status_valid", entry_path, f"{adoption_id} has invalid status {status!r}"))
            errors.append(f"{adoption_id} has invalid adoption status")

        if isinstance(evidence, list) and evidence:
            checks.append(check("pass", "adoption_matrix.evidence_present", entry_path, f"{adoption_id} has evidence"))
            errors.extend(validate_adoption_evidence(repo, adoption_id, evidence, checks))
        else:
            checks.append(check("fail", "adoption_matrix.evidence_present", entry_path, f"{adoption_id} evidence missing"))
            errors.append(f"{adoption_id} adoption evidence missing")

        if isinstance(verified_behaviors, list) and verified_behaviors:
            checks.append(check("pass", "adoption_matrix.behaviors_present", entry_path, f"{adoption_id} has verified behaviors"))
        else:
            checks.append(check("fail", "adoption_matrix.behaviors_present", entry_path, f"{adoption_id} verified behaviors missing"))
            errors.append(f"{adoption_id} verified behaviors missing")

        if isinstance(next_gap, str) and next_gap.strip():
            checks.append(check("pass", "adoption_matrix.next_gap_present", entry_path, f"{adoption_id} has next gap"))
        else:
            checks.append(check("fail", "adoption_matrix.next_gap_present", entry_path, f"{adoption_id} next gap missing"))
            errors.append(f"{adoption_id} next gap missing")

    return checks, errors, warnings


def validate_adoption_evidence(
    repo: Path,
    adoption_id: str,
    evidence: list[object],
    checks: list[dict[str, str]],
) -> list[str]:
    errors: list[str] = []
    for index, item in enumerate(evidence):
        evidence_path = f"{ADOPTION_MATRIX_FIXTURE}#{adoption_id}.evidence[{index}]"
        if not isinstance(item, dict):
            checks.append(check("fail", "adoption_matrix.evidence_shape", evidence_path, "evidence item must be an object"))
            errors.append(f"{adoption_id} evidence item {index} must be an object")
            continue
        kind = item.get("kind")
        if not isinstance(kind, str) or not kind:
            checks.append(check("fail", "adoption_matrix.evidence_shape", evidence_path, "evidence kind missing"))
            errors.append(f"{adoption_id} evidence item {index} missing kind")
            continue
        if kind == "specrail_artifact":
            rel_path = item.get("path")
            if not isinstance(rel_path, str) or not rel_path:
                checks.append(check("fail", "adoption_matrix.local_evidence", evidence_path, "specrail artifact path missing"))
                errors.append(f"{adoption_id} specrail evidence item {index} missing path")
                continue
            path, path_error = repo_relative_file(repo, rel_path)
            if path_error:
                checks.append(check("fail", "adoption_matrix.local_evidence", evidence_path, path_error))
                errors.append(f"{adoption_id} SpecRail evidence path invalid: {rel_path}: {path_error}")
                continue
            if path.is_file():
                checks.append(check("pass", "adoption_matrix.local_evidence", rel_path, "SpecRail evidence path exists"))
            else:
                checks.append(check("fail", "adoption_matrix.local_evidence", rel_path, "SpecRail evidence path missing"))
                errors.append(f"{adoption_id} SpecRail evidence path missing: {rel_path}")
            continue
        if kind in {"github_issue", "github_pr"}:
            if item.get("repo") and item.get("number") and item.get("url"):
                checks.append(check("pass", "adoption_matrix.remote_evidence", evidence_path, f"{kind} pointer recorded"))
            else:
                checks.append(check("fail", "adoption_matrix.remote_evidence", evidence_path, f"{kind} pointer incomplete"))
                errors.append(f"{adoption_id} {kind} evidence item {index} incomplete")
            continue
        if kind in {"external_artifact", "external_local_path"}:
            if item.get("path"):
                checks.append(check("pass", "adoption_matrix.external_path_evidence", evidence_path, "external artifact pointer recorded"))
            else:
                checks.append(check("fail", "adoption_matrix.external_path_evidence", evidence_path, "external artifact pointer missing"))
                errors.append(f"{adoption_id} external artifact evidence item {index} missing path")
            continue
        checks.append(check("fail", "adoption_matrix.evidence_kind", evidence_path, f"unsupported evidence kind {kind}"))
        errors.append(f"{adoption_id} evidence item {index} has unsupported kind {kind}")
    return errors


def evaluate(repo: Path, spec_dir: Path) -> dict[str, object]:
    checks: list[dict[str, str]] = []
    errors: list[str] = []
    warnings: list[str] = []

    try:
        load_pack(repo)
        checks.append(check("pass", "workflow.config_present", "workflow.yaml", "workflow config loaded"))
        checks.append(check("pass", "workflow.states_present", "states.yaml", "state config loaded"))
        checks.append(check("pass", "workflow.labels_present", "labels.yaml", "label config loaded"))
    except SpecRailError as exc:
        checks.append(check("fail", "workflow.config_present", ".", str(exc)))
        errors.append(str(exc))

    spec_checks, spec_errors = evaluate_spec(repo, spec_dir)
    checks.extend(spec_checks)
    errors.extend(spec_errors)
    smoke_checks, smoke_errors, smoke_warnings = evaluate_rclean_smoke(repo)
    checks.extend(smoke_checks)
    errors.extend(smoke_errors)
    warnings.extend(smoke_warnings)
    adoption_checks, adoption_errors, adoption_warnings = evaluate_adoption_matrix(repo)
    checks.extend(adoption_checks)
    errors.extend(adoption_errors)
    warnings.extend(adoption_warnings)

    artifacts = {
        "product_spec": str((spec_dir / "product.md").relative_to(repo)),
        "tech_spec": str((spec_dir / "tech.md").relative_to(repo)),
        "tasks_artifact": str((spec_dir / "tasks.md").relative_to(repo)),
        "smoke_example": "examples/rclean-smoke.md",
        "adoption_matrix": ADOPTION_MATRIX_DOC,
        "adoption_fixture": ADOPTION_MATRIX_FIXTURE,
    }
    if errors:
        status = "fail"
    elif any(item["status"] == "needs_human" for item in checks):
        status = "needs_human"
    else:
        status = "pass"
    next_actions: list[str] = []
    if status == "needs_human":
        next_actions.append("Review rclean draft issue evidence before creating new GitHub issues.")
    if status == "fail":
        next_actions.append("Fix missing or malformed SpecRail artifacts and rerun evaluate.py.")
    return {
        "status": status,
        "repo": str(repo),
        "spec_dir": str(spec_dir.relative_to(repo)),
        "checks": checks,
        "artifacts": artifacts,
        "errors": errors,
        "warnings": warnings,
        "next_actions": next_actions,
    }


def print_text(result: dict[str, object]) -> None:
    print(f"status: {result['status']}")
    print(f"spec_dir: {result['spec_dir']}")
    if result["errors"]:
        print("errors:")
        for error in result["errors"]:
            print(f"- {error}")
    if result["warnings"]:
        print("warnings:")
        for warning in result["warnings"]:
            print(f"- {warning}")
    if result["next_actions"]:
        print("next_actions:")
        for action in result["next_actions"]:
            print(f"- {action}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Evaluate SpecRail artifacts.")
    parser.add_argument("--repo", default=".", help="SpecRail repository root")
    parser.add_argument("--spec-dir", required=True, help="Spec directory to evaluate")
    parser.add_argument("--format", choices=["json", "text"], default="text")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    spec_dir = (repo / args.spec_dir).resolve()
    if not repo.is_dir():
        print(f"error: repo is not a directory: {repo}", file=sys.stderr)
        return 2
    try:
        spec_dir.relative_to(repo)
    except ValueError:
        print(f"error: spec-dir must be inside repo: {spec_dir}", file=sys.stderr)
        return 2

    result = evaluate(repo, spec_dir)
    if args.format == "json":
        print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    else:
        print_text(result)
    return 1 if result["status"] == "fail" else 0


if __name__ == "__main__":
    sys.exit(main())
