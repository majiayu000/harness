#!/usr/bin/env python3
"""Persist PR repair eval artifacts through the Harness eval API."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

from evaluate_pr_repair_artifacts import read_json
from evaluate_pr_repair_submit import load_response, write_json


JsonObject = dict[str, Any]


def http_headers() -> dict[str, str]:
    headers = {"Content-Type": "application/json"}
    token = os.environ.get("HARNESS_API_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def post_json(server_url: str, path: str, body: JsonObject | None) -> tuple[int, JsonObject]:
    data = b"" if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        f"{server_url.rstrip('/')}{path}",
        data=data,
        headers=http_headers(),
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            raw = response.read().decode("utf-8", errors="replace")
            status = response.status
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        status = exc.code
    except (urllib.error.URLError, OSError) as exc:
        reason = getattr(exc, "reason", exc)
        return 0, {
            "error": str(reason),
            "http_status": "000",
            "status": "failed",
        }

    payload = load_response(raw)
    payload["http_status"] = str(status)
    if status < 200 or status >= 300:
        payload.setdefault("status", "failed")
    return status, payload


def eval_run_id(payload: Any) -> str | None:
    if not isinstance(payload, dict):
        return None
    run = payload.get("run")
    if not isinstance(run, dict):
        return None
    run_id = run.get("id")
    if isinstance(run_id, str) and run_id.strip():
        return run_id
    return None


def eval_run_create_body(input_path: Path, source_task_id: str | None) -> JsonObject:
    eval_input = read_json(input_path)
    if not isinstance(eval_input, dict):
        raise SystemExit(f"eval input artifact must be a JSON object: {input_path}")
    scenario = eval_input.get("scenario")
    target = eval_input.get("target")
    if not isinstance(scenario, str) or not scenario.strip():
        raise SystemExit(f"eval input artifact is missing scenario: {input_path}")
    if not isinstance(target, dict):
        raise SystemExit(f"eval input artifact is missing target object: {input_path}")

    body: JsonObject = {
        "scenario": scenario,
        "target": target,
    }
    if source_task_id:
        body["source_task_id"] = source_task_id
    return body


def ensure_eval_run(args: argparse.Namespace) -> int:
    existing = read_json(args.output, required=False)
    existing_id = eval_run_id(existing)
    if existing_id:
        sys.stdout.write(f"{existing_id}\n")
        return 0

    status, payload = post_json(
        args.server_url,
        "/api/evals/runs",
        eval_run_create_body(args.input, args.source_task_id or None),
    )
    write_json(str(args.output), payload)
    run_id = eval_run_id(payload)
    if status < 200 or status >= 300 or not run_id:
        sys.stderr.write(f"failed to create eval run; see {args.output}\n")
        return 22
    sys.stdout.write(f"{run_id}\n")
    return 0


def persist_eval_score(args: argparse.Namespace) -> int:
    run_payload = read_json(args.run)
    run_id = eval_run_id(run_payload)
    if not run_id:
        sys.stderr.write(f"eval run artifact is missing run.id: {args.run}\n")
        return 2

    try:
        input_body = args.input.read_text(encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(f"failed to read eval input artifact {args.input}: {exc}\n")
        return 2
    if not input_body.strip():
        sys.stderr.write(f"eval input artifact is empty: {args.input}\n")
        return 2

    encoded_run_id = urllib.parse.quote(run_id, safe="")
    artifact_status, artifact_payload = post_json(
        args.server_url,
        f"/api/evals/runs/{encoded_run_id}/artifacts",
        {
            "artifact_type": "pr_repair_eval_input",
            "label": "canonical PR repair eval input",
            "content_type": "application/json",
            "body": input_body,
        },
    )
    write_json(str(args.artifact_output), artifact_payload)
    if artifact_status < 200 or artifact_status >= 300:
        sys.stderr.write(f"failed to upload eval input artifact; see {args.artifact_output}\n")
        return 22

    score_status, score_payload = post_json(
        args.server_url,
        f"/api/evals/runs/{encoded_run_id}/score",
        None,
    )
    write_json(str(args.score_output), score_payload)
    if score_status < 200 or score_status >= 300:
        sys.stderr.write(f"failed to score eval run; see {args.score_output}\n")
        return 22
    return 0


def build_persist_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    ensure_run = sub.add_parser("ensure-run")
    ensure_run.add_argument("--server-url", required=True)
    ensure_run.add_argument("--input", type=Path, required=True)
    ensure_run.add_argument("--source-task-id", default="")
    ensure_run.add_argument("--output", type=Path, required=True)
    ensure_run.set_defaults(func=ensure_eval_run)

    persist_score = sub.add_parser("persist-score")
    persist_score.add_argument("--server-url", required=True)
    persist_score.add_argument("--input", type=Path, required=True)
    persist_score.add_argument("--run", type=Path, required=True)
    persist_score.add_argument("--artifact-output", type=Path, required=True)
    persist_score.add_argument("--score-output", type=Path, required=True)
    persist_score.set_defaults(func=persist_eval_score)

    return parser


def main() -> int:
    args = build_persist_parser().parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
