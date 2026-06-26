#!/usr/bin/env python3
"""Collect read-only GitHub PR evidence for the offline SpecRail PR gate."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from typing import Any


PR_VIEW_FIELDS = [
    "number",
    "state",
    "isDraft",
    "headRefOid",
    "mergeStateStatus",
    "closingIssuesReferences",
    "statusCheckRollup",
    "reviews",
]

REVIEW_THREADS_QUERY = """
query SpecRailReviewThreads($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          isOutdated
          comments(first: 5) {
            nodes {
              url
              author {
                login
              }
            }
          }
        }
      }
    }
  }
}
""".strip()

REPO_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
STATUS_CONTEXT_STATES = {"SUCCESS", "FAILURE", "ERROR", "PENDING", "EXPECTED"}


class EvidenceError(ValueError):
    """Raised when GitHub evidence cannot be collected or normalized."""


def parse_github_repo(raw: str) -> tuple[str, str]:
    value = raw.strip()
    if not REPO_PATTERN.fullmatch(value):
        raise EvidenceError("GitHub repository must use OWNER/REPO format")
    owner, name = value.split("/", 1)
    if owner in {".", ".."} or name in {".", ".."}:
        raise EvidenceError("GitHub repository owner and name must be explicit")
    return owner, name


def parse_pr_number(raw: str) -> int:
    try:
        value = int(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("PR number must be a positive integer") from exc
    if value <= 0:
        raise argparse.ArgumentTypeError("PR number must be a positive integer")
    return value


def run_gh_json(args: list[str]) -> dict[str, Any]:
    command = ["gh", *args]
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as exc:
        raise EvidenceError("gh executable was not found in PATH") from exc

    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip() or "no output"
        raise EvidenceError(f"gh command failed: {' '.join(command[:4])}: {detail}")

    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise EvidenceError(f"gh command returned invalid JSON: {exc.msg}") from exc
    if not isinstance(payload, dict):
        raise EvidenceError("gh command JSON output must be an object")
    return payload


def collect_pr_view(github_repo: str, pr_number: int) -> dict[str, Any]:
    return run_gh_json(
        [
            "pr",
            "view",
            str(pr_number),
            "--repo",
            github_repo,
            "--json",
            ",".join(PR_VIEW_FIELDS),
        ]
    )


def collect_review_threads(owner: str, name: str, pr_number: int) -> dict[str, Any]:
    return run_gh_json(
        [
            "api",
            "graphql",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={name}",
            "-F",
            f"number={pr_number}",
            "-f",
            f"query={REVIEW_THREADS_QUERY}",
        ]
    )


def _require_mapping(value: Any, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise EvidenceError(f"{field} must be an object")
    return value


def _require_list(value: Any, field: str) -> list[Any]:
    if not isinstance(value, list):
        raise EvidenceError(f"{field} must be a list")
    return value


def _require_positive_int(payload: dict[str, Any], field: str) -> int:
    value = payload.get(field)
    if not isinstance(value, int) or value <= 0:
        raise EvidenceError(f"{field} must be a positive integer")
    return value


def _require_string(payload: dict[str, Any], field: str) -> str:
    value = payload.get(field)
    if not isinstance(value, str) or not value.strip():
        raise EvidenceError(f"{field} must be a non-empty string")
    return value.strip()


def _require_bool(payload: dict[str, Any], field: str) -> bool:
    value = payload.get(field)
    if not isinstance(value, bool):
        raise EvidenceError(f"{field} must be a boolean")
    return value


def _coerce_optional_positive_int(value: Any) -> int | None:
    if isinstance(value, int) and value > 0:
        return value
    return None


def _first_comment_url(thread: dict[str, Any]) -> str | None:
    comments = thread.get("comments")
    if not isinstance(comments, dict):
        return None
    nodes = comments.get("nodes")
    if not isinstance(nodes, list):
        return None
    for node in nodes:
        if isinstance(node, dict) and isinstance(node.get("url"), str) and node["url"].strip():
            return node["url"].strip()
    return None


def _author_login(value: Any, fallback: str) -> str:
    if isinstance(value, dict) and isinstance(value.get("login"), str) and value["login"].strip():
        return value["login"].strip()
    if isinstance(value, str) and value.strip():
        return value.strip()
    return fallback


def _normalize_status_context(item: dict[str, Any]) -> tuple[str, str]:
    state = str(item.get("state") or "").upper()
    if state not in STATUS_CONTEXT_STATES:
        return "", ""
    if state == "SUCCESS":
        return "COMPLETED", "SUCCESS"
    if state in {"PENDING", "EXPECTED"}:
        return "IN_PROGRESS", ""
    return "COMPLETED", state


def _rollup_items(value: Any) -> list[Any]:
    if isinstance(value, list):
        return value
    if isinstance(value, dict):
        nodes = value.get("nodes")
        if isinstance(nodes, list):
            return nodes
    raise EvidenceError("statusCheckRollup must be a list or nodes object")


def normalize_checks(value: Any) -> list[dict[str, str]]:
    checks: list[dict[str, str]] = []
    for index, item in enumerate(_rollup_items(value), start=1):
        if not isinstance(item, dict):
            raise EvidenceError(f"statusCheckRollup item #{index} must be an object")
        name = str(item.get("name") or item.get("context") or item.get("workflowName") or f"check #{index}")
        status = str(item.get("status") or "").upper()
        conclusion = str(item.get("conclusion") or "").upper()
        if not status and not conclusion:
            status, conclusion = _normalize_status_context(item)
        if not status and conclusion == "SUCCESS":
            status = "COMPLETED"
        check = {
            "name": name,
            "status": status,
            "conclusion": conclusion,
        }
        url = item.get("detailsUrl") or item.get("targetUrl")
        if isinstance(url, str) and url.strip():
            check["url"] = url.strip()
        checks.append(check)
    return checks


def normalize_reviews(value: Any) -> list[dict[str, str]]:
    reviews = _require_list(value, "reviews")
    latest_by_author: dict[str, dict[str, str]] = {}
    author_order: list[str] = []
    for index, item in enumerate(reviews, start=1):
        if not isinstance(item, dict):
            raise EvidenceError(f"review item #{index} must be an object")
        state = str(item.get("state") or "").upper()
        if not state:
            continue
        author = _author_login(item.get("author"), f"review #{index}")
        if author not in latest_by_author:
            author_order.append(author)
        latest_by_author[author] = {"author": author, "state": state}
    return [latest_by_author[author] for author in author_order]


def normalize_review_threads(graphql_payload: dict[str, Any]) -> list[dict[str, Any]]:
    data = _require_mapping(graphql_payload.get("data"), "data")
    repository = _require_mapping(data.get("repository"), "data.repository")
    pull_request = _require_mapping(
        repository.get("pullRequest"), "data.repository.pullRequest"
    )
    review_threads = _require_mapping(
        pull_request.get("reviewThreads"), "data.repository.pullRequest.reviewThreads"
    )
    nodes = _require_list(
        review_threads.get("nodes"), "data.repository.pullRequest.reviewThreads.nodes"
    )

    normalized: list[dict[str, Any]] = []
    for index, item in enumerate(nodes, start=1):
        if not isinstance(item, dict):
            raise EvidenceError(f"review thread item #{index} must be an object")
        thread: dict[str, Any] = {
            "is_resolved": item.get("isResolved") is True,
            "is_outdated": item.get("isOutdated") is True,
        }
        thread_id = item.get("id")
        if isinstance(thread_id, str) and thread_id.strip():
            thread["id"] = thread_id.strip()
        url = _first_comment_url(item)
        if url:
            thread["url"] = url
        normalized.append(thread)
    return normalized


def normalize_linked_issue(value: Any) -> int | None:
    if not isinstance(value, list):
        raise EvidenceError("closingIssuesReferences must be a list")
    for item in value:
        if isinstance(item, dict):
            number = _coerce_optional_positive_int(item.get("number"))
            if number is not None:
                return number
    return None


def build_human_authorization(
    actor: str | None,
    source: str | None,
    summary: str | None,
) -> dict[str, str] | None:
    provided = [value for value in [actor, source, summary] if value is not None and value.strip()]
    if not provided:
        return None
    if not actor or not actor.strip() or not source or not source.strip():
        raise EvidenceError(
            "--authorization-actor and --authorization-source must be provided together"
        )
    authorization = {
        "actor": actor.strip(),
        "source": source.strip(),
    }
    if summary and summary.strip():
        authorization["summary"] = summary.strip()
    return authorization


def build_evidence(
    pr_payload: dict[str, Any],
    threads_payload: dict[str, Any],
    authorization: dict[str, str] | None = None,
) -> dict[str, Any]:
    evidence: dict[str, Any] = {
        "pr": _require_positive_int(pr_payload, "number"),
        "state": _require_string(pr_payload, "state").upper(),
        "is_draft": _require_bool(pr_payload, "isDraft"),
        "head_sha": _require_string(pr_payload, "headRefOid"),
        "merge_state": _require_string(pr_payload, "mergeStateStatus").upper(),
        "linked_issue": normalize_linked_issue(pr_payload.get("closingIssuesReferences")),
        "checks": normalize_checks(pr_payload.get("statusCheckRollup")),
        "reviews": normalize_reviews(pr_payload.get("reviews")),
        "review_threads": normalize_review_threads(threads_payload),
    }
    if authorization is not None:
        evidence["human_authorization"] = authorization
    return evidence


def collect_evidence(
    github_repo: str,
    pr_number: int,
    authorization: dict[str, str] | None,
) -> dict[str, Any]:
    owner, name = parse_github_repo(github_repo)
    pr_payload = collect_pr_view(github_repo, pr_number)
    threads_payload = collect_review_threads(owner, name, pr_number)
    return build_evidence(pr_payload, threads_payload, authorization)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Collect read-only GitHub PR evidence for SpecRail pr_gate.py."
    )
    parser.add_argument("--github-repo", required=True, help="GitHub repository as OWNER/REPO")
    parser.add_argument("--pr", required=True, type=parse_pr_number, help="Pull request number")
    parser.add_argument("--authorization-actor", help="Human authorizing merge")
    parser.add_argument("--authorization-source", help="Where authorization was recorded")
    parser.add_argument("--authorization-summary", help="Short authorization summary")
    parser.add_argument("--json", action="store_true", help="Print JSON output")
    args = parser.parse_args()

    try:
        authorization = build_human_authorization(
            args.authorization_actor,
            args.authorization_source,
            args.authorization_summary,
        )
        evidence = collect_evidence(args.github_repo, args.pr, authorization)
    except EvidenceError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    print(json.dumps(evidence, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
