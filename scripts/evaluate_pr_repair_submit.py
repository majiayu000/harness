#!/usr/bin/env python3
"""Submit a PR repair eval task and preserve non-2xx response bodies."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request


def load_response(raw: str) -> dict[str, object]:
    try:
        data = json.loads(raw) if raw else {}
    except json.JSONDecodeError:
        return {"raw_response": raw}
    if isinstance(data, dict):
        return data
    return {"raw_response": data}


def write_json(path: str, data: dict[str, object]) -> None:
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(data, fh, indent=2, sort_keys=True)
        fh.write("\n")


def write_failed_submission(path: str, mode: str, error: object) -> None:
    data = {
        "error": str(error),
        "eval_submission_mode": mode,
        "http_status": "000",
        "status": "failed",
    }
    write_json(path, data)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--body", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--mode", required=True)
    args = parser.parse_args()

    with open(args.body, "rb") as fh:
        body = fh.read()

    request = urllib.request.Request(
        f"{args.server_url.rstrip('/')}/tasks",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    token = os.environ.get("HARNESS_API_TOKEN")
    if token:
        request.add_header("Authorization", f"Bearer {token}")

    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            raw = response.read().decode("utf-8", errors="replace")
            status = response.status
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        status = exc.code
    except (urllib.error.URLError, OSError) as exc:
        write_failed_submission(args.output, args.mode, getattr(exc, "reason", exc))
        return 7

    data = load_response(raw)
    data["eval_submission_mode"] = args.mode
    data["http_status"] = str(status)
    if status < 200 or status >= 300:
        data.setdefault("status", "failed")
    write_json(args.output, data)
    return 0 if 200 <= status < 300 else 22


if __name__ == "__main__":
    sys.exit(main())
