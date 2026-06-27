#!/usr/bin/env python3
"""Evaluate whether a SpecRail action may proceed from local evidence."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from specrail_lib import (
    TERMINAL_BLOCKING_STATES,
    SpecRailError,
    action_policy,
    infer_state,
    load_pack,
    render_artifact_path,
    state_map,
    validate_action_policy,
    validate_labels,
    validate_state_graph,
)


ROUTE_ALIASES = {
    "action": "triage_issue",
    "triage": "triage_issue",
    "spec": "write_spec",
    "write-spec": "write_spec",
    "write_spec": "write_spec",
    "implement": "implement",
    "review": "review_pr",
    "review-pr": "review_pr",
    "review_pr": "review_pr",
    "fix-ci": "fix_ci",
    "fix_ci": "fix_ci",
    "release-note": "draft_release_note",
    "draft-release-note": "draft_release_note",
    "draft_release_note": "draft_release_note",
}

ARTIFACT_FILES = {
    "product_spec",
    "tech_spec",
    "task_plan",
}


def normalize_route(raw: str) -> str:
    route = ROUTE_ALIASES.get(raw, raw)
    return route.replace("-", "_")


def parse_artifact_value(raw: str) -> tuple[str, str]:
    name, sep, value = raw.partition("=")
    if not sep or not name.strip() or not value.strip():
        raise SpecRailError(f"invalid --artifact {raw!r}; expected name=value")
    return name.strip(), value.strip()


def parse_positive_int(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("value must be a positive integer") from exc
    if value <= 0:
        raise argparse.ArgumentTypeError("value must be a positive integer")
    return value


def load_evidence(path: Path | None) -> dict[str, Any]:
    if path is None:
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise SpecRailError(f"cannot read evidence file {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise SpecRailError(f"invalid evidence JSON {path}: {exc.msg}") from exc
    if not isinstance(data, dict):
        raise SpecRailError("evidence JSON must be an object")
    return data


def artifact_exists(repo: Path, artifact_path: str | None) -> bool:
    if not artifact_path:
        return False
    return (repo / artifact_path).is_file()


def safe_relative_artifact_path(raw: object) -> str | None:
    value = str(raw).strip()
    if not value:
        return None
    path = Path(value)
    if path.is_absolute() or ".." in path.parts:
        return None
    return path.as_posix()


def positive_int(value: object) -> int | None:
    return value if type(value) is int and value > 0 else None


def evidence_labels(evidence: dict[str, Any]) -> list[str]:
    labels = evidence.get("labels", [])
    if labels is None:
        return []
    if not isinstance(labels, list):
        raise SpecRailError("evidence labels must be a list when provided")
    return [str(label) for label in labels if str(label).strip()]


def evidence_route_state(evidence: dict[str, Any], states: dict[str, Any]) -> str | None:
    for key in ["specrail_state", "route_state", "current_state", "state"]:
        value = evidence.get(key)
        if isinstance(value, str) and value in states:
            return value
    return None


def required_artifact_path(
    config: Any,
    artifact: str,
    issue: int | None,
    pr: int | None,
) -> str | None:
    if artifact == "linked_issue":
        return None
    if artifact == "linked_pr":
        return None
    if artifact == "verification":
        return None
    return render_artifact_path(config, artifact, issue, pr)


def evaluate_route(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(args.repo).resolve()
    config = load_pack(repo)
    config_errors: list[str] = []
    config_errors.extend(validate_state_graph(config))
    config_errors.extend(validate_labels(config))
    config_errors.extend(validate_action_policy(config))

    route = normalize_route(args.route)
    policies = action_policy(config)
    policy = policies.get(route)
    if policy is None:
        config_errors.append(f"unknown route: {route}")

    evidence = load_evidence(Path(args.evidence) if args.evidence else None)
    effective_issue = args.issue or positive_int(evidence.get("linked_issue"))
    effective_pr = args.pr or positive_int(evidence.get("pr"))
    labels = list(args.label or [])
    labels.extend(evidence_labels(evidence))
    states = state_map(config)
    explicit_state = args.state or evidence_route_state(evidence, states)

    reasons: list[str] = []
    satisfied: list[str] = []
    missing: list[str] = []
    blocked_actions: list[str] = []
    allowed_actions: list[str] = []
    required_artifacts: list[str] = []
    human_gates: list[str] = []

    if config_errors:
        return {
            "decision": "blocked",
            "route": route,
            "current_state": explicit_state,
            "issue": args.issue,
            "pr": args.pr,
            "reasons": config_errors,
            "satisfied": [],
            "missing": [],
            "required_artifacts": [],
            "human_gates": [],
            "allowed_actions": [],
            "blocked_actions": [route],
            "verification_commands": ["python3 checks/check_workflow.py --repo ."],
        }

    current_state, state_evidence = infer_state(config, explicit_state, labels)
    satisfied.extend(state_evidence)

    if current_state and current_state not in states:
        return blocked_result(
            route,
            current_state,
            args,
            [f"unknown current state: {current_state}"],
        )

    if current_state in TERMINAL_BLOCKING_STATES:
        return blocked_result(
            route,
            current_state,
            args,
            [f"state {current_state} is terminal or maintainer-reserved"],
        )

    assert policy is not None
    allowed_from = [str(state) for state in policy.get("allowed_from", [])]
    required = [str(artifact) for artifact in policy.get("required_artifacts", [])]
    creates = [str(artifact) for artifact in policy.get("creates_artifacts", [])]
    human_gates = [str(gate) for gate in policy.get("human_gates", [])]

    if current_state is None:
        missing.append("current_state")
        reasons.append("no state or matching readiness label was provided")
    elif current_state in allowed_from:
        satisfied.append(f"state {current_state} allows {route}")
    else:
        missing.append(f"allowed_state:{'|'.join(allowed_from)}")
        reasons.append(
            f"route {route} requires one of {', '.join(allowed_from)}; got {current_state}"
        )

    provided_artifacts = dict(evidence.get("artifacts", {})) if isinstance(evidence.get("artifacts"), dict) else {}
    for raw_artifact in args.artifact or []:
        name, value = parse_artifact_value(raw_artifact)
        provided_artifacts[name] = value

    for artifact in required:
        path = required_artifact_path(config, artifact, effective_issue, effective_pr)
        if artifact == "linked_issue":
            if effective_issue is None:
                missing.append("linked_issue")
            else:
                satisfied.append(f"linked_issue: GH-{effective_issue}")
            continue
        if artifact == "linked_pr":
            if effective_pr is None:
                missing.append("linked_pr")
            else:
                satisfied.append(f"linked_pr: PR-{effective_pr}")
            continue
        if artifact == "verification":
            verification = evidence.get("verification") or provided_artifacts.get("verification")
            if verification:
                satisfied.append("verification evidence provided")
            else:
                missing.append("verification")
            continue
        required_artifacts.append(path or artifact)
        provided = provided_artifacts.get(artifact)
        if provided:
            provided_path = safe_relative_artifact_path(provided)
            if provided_path is None:
                missing.append(f"{artifact}:{provided}")
            elif path and provided_path != path:
                missing.append(f"{artifact}:{provided_path}:expected:{path}")
            elif artifact in ARTIFACT_FILES and not (repo / provided_path).is_file():
                missing.append(f"{artifact}:{provided_path}")
            else:
                satisfied.append(f"{artifact}: {provided_path}")
            continue
        if artifact in ARTIFACT_FILES:
            if artifact_exists(repo, path):
                satisfied.append(f"{artifact}: {path}")
            else:
                missing.append(f"{artifact}:{path}")
        elif path:
            required_artifacts.append(path)

    for action, action_body in policies.items():
        allowed_states = [str(state) for state in action_body.get("allowed_from", [])]
        if current_state and current_state in allowed_states:
            allowed_actions.append(action)

    blocked_actions.extend(["final_approval", "merge", "force_push"])

    if missing:
        if current_state is None or any(item.startswith("allowed_state:") for item in missing):
            decision = "needs_human" if human_gates else "blocked"
        else:
            decision = "warn" if args.mode in {"dry_run", "advisory"} else "blocked"
    else:
        decision = "allowed"
        reasons.append(f"route {route} passed local SpecRail gates")

    if decision in {"blocked", "needs_human"}:
        blocked_actions.append(route)
        allowed_actions = [action for action in allowed_actions if action != route]

    for artifact in creates:
        path = render_artifact_path(config, artifact, effective_issue, effective_pr)
        if path:
            required_artifacts.append(path)

    return {
        "decision": decision,
        "route": route,
        "mode": args.mode,
        "current_state": current_state,
        "issue": effective_issue,
        "pr": effective_pr,
        "reasons": reasons,
        "satisfied": sorted(set(satisfied)),
        "missing": sorted(set(missing)),
        "required_artifacts": sorted(set(required_artifacts)),
        "human_gates": human_gates,
        "allowed_actions": sorted(set(allowed_actions)),
        "blocked_actions": sorted(set(blocked_actions)),
        "verification_commands": [
            "python3 checks/check_workflow.py --repo .",
            *(
                [f"python3 checks/check_workflow.py --repo . --spec-dir specs/GH{effective_issue}"]
                if effective_issue
                else []
            ),
        ],
    }


def blocked_result(
    route: str,
    current_state: str | None,
    args: argparse.Namespace,
    reasons: list[str],
) -> dict[str, Any]:
    return {
        "decision": "blocked",
        "route": route,
        "mode": args.mode,
        "current_state": current_state,
        "issue": args.issue,
        "pr": args.pr,
        "reasons": reasons,
        "satisfied": [],
        "missing": [],
        "required_artifacts": [],
        "human_gates": [],
        "allowed_actions": [],
        "blocked_actions": [route],
        "verification_commands": ["python3 checks/check_workflow.py --repo ."],
    }


def print_human(result: dict[str, Any]) -> None:
    print(f"decision: {result['decision']}")
    print(f"route: {result['route']}")
    if result.get("current_state"):
        print(f"current_state: {result['current_state']}")
    if result.get("issue"):
        print(f"issue: GH-{result['issue']}")
    if result.get("pr"):
        print(f"pr: PR-{result['pr']}")
    if result.get("reasons"):
        print("reasons:")
        for reason in result["reasons"]:
            print(f"- {reason}")
    if result.get("missing"):
        print("missing:")
        for item in result["missing"]:
            print(f"- {item}")
    if result.get("required_artifacts"):
        print("required_artifacts:")
        for item in result["required_artifacts"]:
            print(f"- {item}")
    if result.get("verification_commands"):
        print("verification_commands:")
        for command in result["verification_commands"]:
            print(f"- {command}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Evaluate a SpecRail route from local evidence."
    )
    parser.add_argument("--repo", default=".", help="SpecRail pack or adopted repo root")
    parser.add_argument("--route", "--action", required=True, help="Route/action to evaluate")
    parser.add_argument("--issue", type=parse_positive_int, help="Linked GitHub issue number")
    parser.add_argument("--pr", type=parse_positive_int, help="Linked pull request number")
    parser.add_argument("--state", help="Canonical SpecRail state")
    parser.add_argument("--label", action="append", default=[], help="Issue/PR label evidence")
    parser.add_argument(
        "--artifact",
        action="append",
        default=[],
        help="Artifact evidence in name=path form",
    )
    parser.add_argument("--evidence", help="Optional JSON evidence file")
    parser.add_argument(
        "--mode",
        default="dry_run",
        choices=["dry_run", "advisory", "required"],
        help="Evaluation enforcement mode",
    )
    parser.add_argument("--json", action="store_true", help="Print JSON output")
    args = parser.parse_args()

    try:
        result = evaluate_route(args)
    except SpecRailError as exc:
        result = {
            "decision": "blocked",
            "route": normalize_route(args.route),
            "mode": args.mode,
            "current_state": args.state,
            "issue": args.issue,
            "pr": args.pr,
            "reasons": [str(exc)],
            "satisfied": [],
            "missing": [],
            "required_artifacts": [],
            "human_gates": [],
            "allowed_actions": [],
            "blocked_actions": [normalize_route(args.route)],
            "verification_commands": ["python3 checks/check_workflow.py --repo ."],
        }

    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        print_human(result)

    if result["decision"] == "blocked":
        return 1
    if result["decision"] == "needs_human" and args.mode == "required":
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
