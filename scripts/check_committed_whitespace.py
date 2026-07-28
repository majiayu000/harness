#!/usr/bin/env python3
"""Check committed whitespace for the revision range in a GitHub Actions event."""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any


OBJECT_ID = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})")


class WhitespaceCheckError(Exception):
    """Raised when the workflow event cannot define a safe diff range."""


def load_event(environment: Mapping[str, str]) -> tuple[str, dict[str, Any]]:
    event_name = environment.get("GITHUB_EVENT_NAME")
    event_path = environment.get("GITHUB_EVENT_PATH")
    if not event_name:
        raise WhitespaceCheckError("GITHUB_EVENT_NAME is required")
    if not event_path:
        raise WhitespaceCheckError("GITHUB_EVENT_PATH is required")

    try:
        payload = json.loads(Path(event_path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise WhitespaceCheckError(f"cannot read GitHub event payload: {error}") from error
    if not isinstance(payload, dict):
        raise WhitespaceCheckError("GitHub event payload must be a JSON object")
    return event_name, payload


def require_object_id(value: object, label: str) -> str:
    if not isinstance(value, str) or OBJECT_ID.fullmatch(value) is None:
        raise WhitespaceCheckError(f"{label} must be a full Git object id")
    if set(value) == {"0"}:
        raise WhitespaceCheckError(f"{label} must not be the zero object id")
    return value


def commit_exists(revision: str) -> bool:
    result = subprocess.run(
        ["git", "cat-file", "-e", f"{revision}^{{commit}}"],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return result.returncode == 0


def select_diff_command(event_name: str, payload: Mapping[str, Any]) -> list[str]:
    if event_name == "pull_request":
        pull_request = payload.get("pull_request")
        if not isinstance(pull_request, Mapping):
            raise WhitespaceCheckError("pull_request payload is required")
        base = pull_request.get("base")
        head = pull_request.get("head")
        if not isinstance(base, Mapping) or not isinstance(head, Mapping):
            raise WhitespaceCheckError(
                "pull_request base and head payloads are required"
            )
        base_sha = require_object_id(base.get("sha"), "pull_request.base.sha")
        head_sha = require_object_id(head.get("sha"), "pull_request.head.sha")
        for revision, label in (
            (base_sha, "pull_request.base.sha"),
            (head_sha, "pull_request.head.sha"),
        ):
            if not commit_exists(revision):
                raise WhitespaceCheckError(f"{label} is not available in the checkout")
        return ["git", "diff", "--check", f"{base_sha}...{head_sha}"]

    if event_name == "push":
        before_sha = require_object_id(payload.get("before"), "push.before")
        after_sha = require_object_id(payload.get("after"), "push.after")
        for revision, label in (
            (before_sha, "push.before"),
            (after_sha, "push.after"),
        ):
            if not commit_exists(revision):
                raise WhitespaceCheckError(f"{label} is not available in the checkout")
        return ["git", "diff", "--check", before_sha, after_sha]

    raise WhitespaceCheckError(f"unsupported GitHub event: {event_name}")


def main(
    argv: Sequence[str] | None = None,
    environment: Mapping[str, str] | None = None,
) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments:
        print("check_committed_whitespace.py does not accept arguments", file=sys.stderr)
        return 2

    try:
        event_name, payload = load_event(
            os.environ if environment is None else environment
        )
        command = select_diff_command(event_name, payload)
        return subprocess.run(command, check=False).returncode
    except (OSError, WhitespaceCheckError) as error:
        print(f"committed whitespace check failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
