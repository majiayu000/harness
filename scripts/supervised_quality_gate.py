#!/usr/bin/env python3
"""Native quality-gate helpers for the supervised Docker host.

Binds a retained candidate Git bundle to command.expected_head_sha. This module
does not advertise eval capabilities or invent resource-limit enforcement.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

QUALITY_GATE_ACTIVITY = "run_quality_gate"
QUALITY_PASSED_SIGNAL = "QualityPassed"
QUALITY_FAILED_SIGNAL = "QualityFailed"

OFFLINE_VALIDATION_SCRIPT = """import hashlib,json,subprocess,sys,time
from pathlib import Path
spec=json.loads(sys.argv[1])
subprocess.run(['python3','-I','/git-handoff.py','verify',
'/handoff/candidate.bundle',spec['base'],spec['candidate'],spec['digest'],
'/handoff/workspace','/candidate'],check=True)
results=[]
for argv in spec['commands']:
  started=time.monotonic()
  proc=subprocess.run(argv,cwd='/candidate',capture_output=True)
  output=proc.stdout+proc.stderr
  results.append({'argv':argv,'exit_code':proc.returncode,
 'output_sha256':hashlib.sha256(output).hexdigest(),
 'duration_ms':int((time.monotonic()-started)*1000)})
Path('/out/validation.json').write_text(json.dumps(results))
sys.exit(0 if all(item['exit_code']==0 for item in results) else 1)
"""


def require_expected_head(command: dict) -> str:
    expected = command.get("expected_head_sha")
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{40}", expected):
        raise RuntimeError("native quality gate requires command.expected_head_sha")
    return expected


def require_validation_commands(command: dict) -> list[list[str]]:
    argv_list = command.get("validation_commands_argv")
    if not isinstance(argv_list, list) or not argv_list:
        raise RuntimeError("native quality gate requires validation_commands_argv")
    commands = []
    for argv in argv_list:
        if not isinstance(argv, list) or not argv or not all(
            isinstance(part, str) and part for part in argv
        ):
            raise RuntimeError("native quality gate validation argv is invalid")
        commands.append(list(argv))
    return commands


def bind_retained_candidate(handoff: dict, expected_head_sha: str) -> None:
    if handoff.get("candidate_commit") != expected_head_sha:
        raise RuntimeError(
            "retained candidate commit does not match command.expected_head_sha; "
            "local candidate SHA is not an externally verified PR head"
        )


def validation_spec(handoff: dict, expected_head_sha: str, commands: list[list[str]]) -> str:
    return json.dumps({
        "base": handoff["base_commit"],
        "candidate": expected_head_sha,
        "digest": handoff["bundle_sha256"],
        "commands": commands,
    })


def read_validation_evidence(path: Path, expected_count: int) -> list:
    if not path.is_file():
        raise RuntimeError("native quality gate validation evidence is missing")
    validation = json.loads(path.read_text())
    if not isinstance(validation, list) or len(validation) != expected_count:
        raise RuntimeError("native quality gate validation evidence is incomplete")
    return validation


def execution_evidence(checked_out_commit: str, validation: list) -> dict:
    # Lease-only client: do not invent eval resource enforcement measurements.
    return {
        "checked_out_commit": checked_out_commit,
        "resource_limit_report": {
            "limits": {"requested": {}, "effective": {}, "caps": []},
            "usage": {},
            "reason": "lease-only supervised client; eval_resource_limits not advertised",
        },
        "usage": {
            "model": "",
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_input_tokens": 0,
            "total_tokens": 0,
            "cost_usd_micros": None,
        },
        "isolation_cleanup_status": "cleaned",
        "validation": validation,
    }


def activity_result(expected_head_sha: str, candidate_commit: str, validation: list) -> dict:
    passed = all(item["exit_code"] == 0 for item in validation)
    status = "succeeded" if passed else "failed"
    signal = QUALITY_PASSED_SIGNAL if passed else QUALITY_FAILED_SIGNAL
    summary = (
        "Native quality gate matched expected_head_sha and validation passed."
        if passed
        else "Native quality gate validation failed against expected_head_sha."
    )
    return {
        "activity": QUALITY_GATE_ACTIVITY,
        "status": status,
        "summary": summary,
        "artifacts": [],
        "signals": (
            [{
                "signal_type": signal,
                "signal": {
                    "expected_head_sha": expected_head_sha,
                    "candidate_commit": candidate_commit,
                },
            }]
            if passed
            else []
        ),
        "validation": [
            {
                "command": " ".join(item["argv"]),
                "status": "passed" if item["exit_code"] == 0 else "failed",
            }
            for item in validation
        ],
        "error": None if passed else "revision-bound validation command failed",
        "error_kind": None if passed else "fatal",
    }
