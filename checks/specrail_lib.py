"""Shared deterministic helpers for SpecRail checks.

This module intentionally avoids third-party dependencies so the default pack
can validate in a fresh repository checkout.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any


DECISIONS = {"allowed", "warn", "needs_human", "blocked"}
AUTOMATION_MODES = {"dry_run", "advisory", "required"}
UNIVERSAL_FORBIDDEN_AGENT_ACTIONS = {
    "final_approval",
    "merge",
    "force_push",
    "close_disputed_issue",
    "public_security_disclosure",
    "permission_change",
}


class SpecRailError(ValueError):
    """Raised when SpecRail configuration or evidence is malformed."""


@dataclass(frozen=True)
class PackConfig:
    repo: Path
    workflow: dict[str, Any]
    states: dict[str, Any]
    labels: dict[str, Any]


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SpecRailError(f"cannot read {path}: {exc}") from exc


def parse_scalar(value: str) -> Any:
    value = value.strip()
    if value in {"true", "True"}:
        return True
    if value in {"false", "False"}:
        return False
    if value in {"null", "None", "~"}:
        return None
    if value == "[]":
        return []
    if value.startswith("[") and value.endswith("]"):
        inner = value[1:-1].strip()
        if not inner:
            return []
        return [parse_scalar(item.strip()) for item in inner.split(",")]
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    if re.fullmatch(r"-?[0-9]+", value):
        return int(value)
    return value


def _significant_lines(text: str) -> list[tuple[int, str]]:
    lines: list[tuple[int, str]] = []
    for raw in text.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        lines.append((indent, raw.strip()))
    return lines


def parse_yaml_subset(text: str) -> Any:
    """Parse the small YAML subset used by SpecRail config files.

    Supported constructs: nested mappings, lists of scalars, booleans, nulls,
    and integers. This is not a general YAML parser.
    """

    lines = _significant_lines(text)

    def parse_block(index: int, indent: int) -> tuple[Any, int]:
        if index >= len(lines):
            return {}, index
        container: Any = [] if lines[index][1].startswith("- ") else {}

        while index < len(lines):
            line_indent, content = lines[index]
            if line_indent < indent:
                break
            if line_indent > indent:
                raise SpecRailError(f"unexpected indent near: {content}")

            if isinstance(container, list):
                if not content.startswith("- "):
                    break
                item = content[2:].strip()
                if not item:
                    child, index = parse_block(index + 1, indent + 2)
                    container.append(child)
                else:
                    container.append(parse_scalar(item))
                    index += 1
                continue

            if content.startswith("- "):
                break
            key, sep, value = content.partition(":")
            if not sep:
                raise SpecRailError(f"expected key/value near: {content}")
            key = key.strip()
            value = value.strip()
            if value:
                container[key] = parse_scalar(value)
                index += 1
                continue
            if index + 1 < len(lines) and lines[index + 1][0] > line_indent:
                child, index = parse_block(index + 1, lines[index + 1][0])
                container[key] = child
            else:
                container[key] = {}
                index += 1
        return container, index

    parsed, end = parse_block(0, lines[0][0] if lines else 0)
    if end != len(lines):
        raise SpecRailError(f"could not parse YAML near: {lines[end][1]}")
    return parsed


def load_yaml_file(path: Path) -> Any:
    return parse_yaml_subset(read_text(path))


def load_pack(repo: Path) -> PackConfig:
    repo = repo.resolve()
    return PackConfig(
        repo=repo,
        workflow=load_yaml_file(repo / "workflow.yaml"),
        states=load_yaml_file(repo / "states.yaml"),
        labels=load_yaml_file(repo / "labels.yaml"),
    )


def state_map(config: PackConfig) -> dict[str, Any]:
    states = config.states.get("states")
    if not isinstance(states, dict):
        raise SpecRailError("states.yaml must contain a states mapping")
    return states


def terminal_states(config: PackConfig) -> set[str]:
    states = state_map(config)
    return {
        str(name)
        for name, body in states.items()
        if isinstance(body, dict) and body.get("terminal") is True
    }


def label_groups(config: PackConfig) -> dict[str, list[str]]:
    labels = config.labels.get("labels")
    if not isinstance(labels, dict):
        raise SpecRailError("labels.yaml must contain a labels mapping")
    groups: dict[str, list[str]] = {}
    for group, values in labels.items():
        groups[group] = [str(value) for value in values] if isinstance(values, list) else []
    return groups


def state_label_map(config: PackConfig) -> dict[str, str]:
    states = state_map(config)
    mapping = {state: state for state in states}
    raw = config.labels.get("state_labels", {})
    if raw is None:
        return mapping
    if not isinstance(raw, dict):
        raise SpecRailError("labels.yaml state_labels must be a mapping")
    for state, labels in raw.items():
        state_name = str(state)
        if state_name not in states:
            continue
        if not isinstance(labels, list):
            continue
        for label in labels:
            label_name = str(label).strip()
            if label_name:
                existing_state = mapping.get(label_name)
                if existing_state is not None and existing_state != state_name:
                    raise SpecRailError(
                        "labels.yaml: state label "
                        f"{label_name} maps to both {existing_state} and {state_name}"
                    )
                mapping[label_name] = state_name
    return mapping


def action_policy(config: PackConfig) -> dict[str, Any]:
    policy = config.workflow.get("action_policy", {})
    actions = policy.get("actions", {}) if isinstance(policy, dict) else {}
    if not isinstance(actions, dict):
        raise SpecRailError("workflow.yaml action_policy.actions must be a mapping")
    return actions


def forbidden_agent_actions(config: PackConfig) -> list[str]:
    policy = config.workflow.get("automation_policy", {})
    if not isinstance(policy, dict):
        raise SpecRailError("workflow.yaml automation_policy must be a mapping")
    actions = policy.get("forbidden_agent_actions", [])
    if not isinstance(actions, list):
        raise SpecRailError("workflow.yaml automation_policy.forbidden_agent_actions must be a list")
    configured_actions = {str(action) for action in actions if str(action).strip()}
    return sorted(UNIVERSAL_FORBIDDEN_AGENT_ACTIONS | configured_actions)


def validate_automation_policy(config: PackConfig) -> list[str]:
    errors: list[str] = []
    policy = config.workflow.get("automation_policy", {})
    if not isinstance(policy, dict):
        return ["workflow.yaml automation_policy must be a mapping"]
    mode = policy.get("default_mode", "dry_run")
    if not isinstance(mode, str) or mode not in AUTOMATION_MODES:
        errors.append(
            "workflow.yaml automation_policy.default_mode must be one of "
            "advisory, dry_run, required"
        )
    try:
        forbidden_agent_actions(config)
    except SpecRailError as exc:
        errors.append(str(exc))
    return errors


def _validate_artifact_template_value(artifact: str, value: Any) -> str:
    if not isinstance(value, str):
        raise SpecRailError(f"workflow.yaml artifacts.{artifact} must be a string")
    template = value.strip()
    if not template:
        raise SpecRailError(f"workflow.yaml artifacts.{artifact} must not be empty")
    path = PurePosixPath(template)
    if path.is_absolute():
        raise SpecRailError(f"workflow.yaml artifacts.{artifact} must be repo-relative")
    if ".." in path.parts:
        raise SpecRailError(f"workflow.yaml artifacts.{artifact} must not contain '..'")
    return template


def artifact_templates(config: PackConfig) -> dict[str, str]:
    artifacts = config.workflow.get("artifacts", {})
    if not isinstance(artifacts, dict):
        raise SpecRailError("workflow.yaml artifacts must be a mapping")
    templates: dict[str, str] = {}
    for key, value in artifacts.items():
        artifact = str(key).strip()
        if not artifact:
            raise SpecRailError("workflow.yaml artifacts keys must not be empty")
        templates[artifact] = _validate_artifact_template_value(artifact, value)
    return templates


def validate_artifact_templates(config: PackConfig) -> list[str]:
    try:
        artifact_templates(config)
    except SpecRailError as exc:
        return [str(exc)]
    return []


def work_id_for_issue(issue: int | None) -> str | None:
    if issue is None:
        return None
    return f"GH{issue}"


def render_artifact_path(
    config: PackConfig,
    artifact: str,
    issue: int | None,
    pr: int | None = None,
) -> str | None:
    template = artifact_templates(config).get(artifact)
    if not template:
        return None
    if issue is None and (
        "{issue_number}" in template or "{work_id}" in template
    ):
        return None
    if pr is None and "{pr_number}" in template:
        return None
    if issue is not None:
        template = template.replace("{issue_number}", str(issue)).replace(
            "{work_id}", work_id_for_issue(issue) or ""
        )
    if pr is not None:
        template = template.replace("{pr_number}", str(pr))
    return template


def infer_state(config: PackConfig, state: str | None, labels: list[str]) -> tuple[str | None, list[str]]:
    if state:
        return state, [f"state provided explicitly: {state}"]

    label_to_state = state_label_map(config)
    label_set = {label.strip() for label in labels if label.strip()}
    matches = sorted({label_to_state[label] for label in label_set if label in label_to_state})
    if len(matches) == 1:
        return matches[0], [f"state inferred from label: {matches[0]}"]
    if len(matches) > 1:
        raise SpecRailError(f"conflicting state labels: {', '.join(matches)}")
    return None, []


def validate_state_graph(config: PackConfig) -> list[str]:
    errors: list[str] = []
    states = state_map(config)
    for name, body in states.items():
        if not isinstance(body, dict):
            errors.append(f"states.yaml: state {name} must be a mapping")
            continue
        if "owner" not in body:
            errors.append(f"states.yaml: state {name} missing owner")
        next_states = body.get("next", [])
        if body.get("terminal") is True and next_states:
            errors.append(f"states.yaml: terminal state {name} must not define next")
        if next_states and not isinstance(next_states, list):
            errors.append(f"states.yaml: state {name} next must be a list")
            continue
        for next_state in next_states:
            if str(next_state) not in states:
                errors.append(f"states.yaml: state {name} references unknown next state {next_state}")
    return errors


def validate_labels(config: PackConfig) -> list[str]:
    errors: list[str] = []
    states = set(state_map(config))
    groups = label_groups(config)
    try:
        label_to_state = state_label_map(config)
    except SpecRailError as exc:
        errors.append(str(exc))
        label_to_state = {state: state for state in states}
    for required_group in ["readiness", "outcome", "review"]:
        if required_group not in groups:
            errors.append(f"labels.yaml: missing label group {required_group}")
    raw_state_labels = config.labels.get("state_labels", {})
    if raw_state_labels is not None and isinstance(raw_state_labels, dict):
        for state, labels in raw_state_labels.items():
            state_name = str(state)
            if state_name not in states:
                errors.append(f"labels.yaml: state_labels references unknown state {state_name}")
            if not isinstance(labels, list):
                errors.append(f"labels.yaml: state_labels.{state_name} must be a list")
    readiness_states = {label_to_state.get(label, label) for label in groups.get("readiness", [])}
    for state in ["needs_info", "triaged", "ready_to_spec", "ready_to_implement"]:
        if state not in readiness_states:
            errors.append(f"labels.yaml: readiness labels missing {state}")
    for label in groups.get("readiness", []) + groups.get("outcome", []):
        mapped = label_to_state.get(label, label)
        if mapped not in states and mapped not in {"merged"}:
            errors.append(f"labels.yaml: label {label} is not a known state or allowed outcome")
    return errors


def validate_action_policy(config: PackConfig) -> list[str]:
    errors: list[str] = []
    states = set(state_map(config))
    artifact_errors = validate_artifact_templates(config)
    if artifact_errors:
        return artifact_errors
    artifacts = set(artifact_templates(config)) | {"linked_issue", "linked_pr", "ci_evidence"}
    configured_human_gates: set[str] = set()
    raw_human_gates = config.workflow.get("required_human_gates", [])
    if not isinstance(raw_human_gates, list):
        errors.append("workflow.yaml: required_human_gates must be a list")
    else:
        configured_human_gates = {str(gate) for gate in raw_human_gates}
    actions = action_policy(config)
    for route in ["triage_issue", "write_spec", "implement", "review_pr", "fix_ci", "draft_release_note"]:
        if route not in actions:
            errors.append(f"workflow.yaml: action_policy missing route {route}")
    for route, body in actions.items():
        if not isinstance(body, dict):
            errors.append(f"workflow.yaml: action {route} must be a mapping")
            continue
        allowed_from = body.get("allowed_from", [])
        if not isinstance(allowed_from, list):
            errors.append(f"workflow.yaml: action {route} allowed_from must be a list")
            continue
        for state in allowed_from:
            if str(state) not in states:
                errors.append(f"workflow.yaml: action {route} references unknown state {state}")
        for field in ["required_artifacts", "creates_artifacts"]:
            artifact_names = body.get(field, [])
            if not isinstance(artifact_names, list):
                errors.append(f"workflow.yaml: action {route} {field} must be a list")
                continue
            for artifact in artifact_names:
                artifact_name = str(artifact)
                if artifact_name not in artifacts:
                    errors.append(
                        f"workflow.yaml: action {route} references unknown artifact {artifact_name}"
                    )
        human_gates = body.get("human_gates", [])
        if not isinstance(human_gates, list):
            errors.append(f"workflow.yaml: action {route} human_gates must be a list")
            continue
        for gate in human_gates:
            gate_name = str(gate)
            if gate_name not in configured_human_gates:
                errors.append(
                    f"workflow.yaml: action {route} references unknown human gate {gate_name}"
                )
    return errors


def validate_template_parity(repo: Path) -> list[str]:
    errors: list[str] = []
    root = repo / "templates"
    zh = root / "zh-CN"
    if not zh.is_dir():
        return errors
    base_files = sorted(path.name for path in root.glob("*.md"))
    zh_files = sorted(path.name for path in zh.glob("*.md")) if zh.is_dir() else []
    for name in base_files:
        if name not in zh_files:
            errors.append(f"templates/zh-CN: missing localized template {name}")
    for name in zh_files:
        if name not in base_files:
            errors.append(f"templates/zh-CN/{name}: no matching base template")
    stable_tokens = ["GH-", "ready_to_spec", "ready_to_implement"]
    for name in ["issue_feature.md", "product_spec.md", "tech_spec.md", "pull_request.md"]:
        for rel in [Path("templates") / name, Path("templates/zh-CN") / name]:
            path = repo / rel
            if not path.is_file():
                continue
            text = read_text(path)
            for token in stable_tokens:
                if token in read_text(repo / "templates" / name) and token not in text:
                    errors.append(f"{rel}: missing stable token {token}")
    return errors


def validate_json_schemas(repo: Path) -> list[str]:
    errors: list[str] = []
    schema_dir = repo / "schemas"
    if not schema_dir.is_dir():
        return ["missing schemas/ directory"]
    for path in sorted(schema_dir.glob("*.schema.json")):
        try:
            data = json.loads(read_text(path))
        except json.JSONDecodeError as exc:
            errors.append(f"{path.relative_to(repo)}: invalid JSON: {exc.msg}")
            continue
        if not isinstance(data, dict):
            errors.append(f"{path.relative_to(repo)}: JSON root must be an object")
            continue
        if "$schema" not in data:
            errors.append(f"{path.relative_to(repo)}: missing $schema")
        if "title" not in data:
            errors.append(f"{path.relative_to(repo)}: missing title")
        if data.get("type") != "object":
            errors.append(f"{path.relative_to(repo)}: top-level type must be object")
    return errors
