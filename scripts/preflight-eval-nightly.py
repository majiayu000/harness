#!/usr/bin/env python3
"""Fail-closed preflight for the scheduled live eval runner."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


REQUIRED_RUNTIME_HOST_CAPABILITIES = {
    "eval_resource_limits",
    "trusted_eval_verifier_v1",
}


class PreflightError(RuntimeError):
    """A missing prerequisite that would make a live eval untrustworthy."""


def _read_json(url: str, api_token: str | None) -> dict[str, Any]:
    headers = {"Accept": "application/json"}
    if api_token:
        headers["Authorization"] = f"Bearer {api_token}"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            payload = response.read()
    except (urllib.error.URLError, TimeoutError) as error:
        raise PreflightError(f"cannot reach Harness control plane at {url}: {error}") from error
    try:
        value = json.loads(payload)
    except json.JSONDecodeError as error:
        raise PreflightError(f"Harness control plane returned invalid JSON at {url}") from error
    if not isinstance(value, dict):
        raise PreflightError(f"Harness control plane returned a non-object response at {url}")
    return value


def validate_preflight(
    *,
    server_url: str,
    gate_mode: str,
    manifest: Path,
    baseline: Path,
    database_url: str | None,
    api_token: str | None,
) -> None:
    if not database_url:
        raise PreflightError("HARNESS_DATABASE_URL is required for live eval execution")
    if gate_mode not in {"report-only", "enforce"}:
        raise PreflightError("gate mode must be report-only or enforce")
    if not manifest.is_file():
        raise PreflightError(f"eval manifest does not exist: {manifest}")
    if gate_mode == "enforce" and not baseline.is_file():
        raise PreflightError(
            f"enforced eval gating requires a reviewed baseline: {baseline}"
        )

    base = server_url.rstrip("/")
    _read_json(f"{base}/health", api_token)
    runtime_hosts = _read_json(f"{base}/api/runtime-hosts", api_token).get("hosts")
    if not isinstance(runtime_hosts, list):
        raise PreflightError("runtime-host response does not contain a hosts array")

    compatible_hosts = []
    for host in runtime_hosts:
        if not isinstance(host, dict) or host.get("online") is not True:
            continue
        if host.get("lifecycle", "active") != "active":
            continue
        capabilities = host.get("capabilities")
        if not isinstance(capabilities, list):
            continue
        if REQUIRED_RUNTIME_HOST_CAPABILITIES.issubset(set(capabilities)):
            compatible_hosts.append(host)
    if not compatible_hosts:
        required = ", ".join(sorted(REQUIRED_RUNTIME_HOST_CAPABILITIES))
        raise PreflightError(
            "no online active runtime host advertises required eval capabilities: "
            f"{required}"
        )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-url", required=True)
    parser.add_argument("--gate-mode", required=True)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--baseline", required=True, type=Path)
    return parser


def main() -> int:
    args = _parser().parse_args()
    try:
        validate_preflight(
            server_url=args.server_url,
            gate_mode=args.gate_mode,
            manifest=args.manifest,
            baseline=args.baseline,
            database_url=os.environ.get("HARNESS_DATABASE_URL"),
            api_token=os.environ.get("HARNESS_API_TOKEN"),
        )
    except PreflightError as error:
        print(f"eval nightly preflight failed: {error}", file=sys.stderr)
        return 1
    print("eval nightly preflight passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
