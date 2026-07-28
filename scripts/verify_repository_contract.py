#!/usr/bin/env python3
"""Verify that the trusted repository-contract suite actually executed."""

from __future__ import annotations

import os
import re
import xml.etree.ElementTree as ET
from pathlib import Path


_COLLECTION_SUMMARY = re.compile(
    r"(?m)^(\d+) tests? collected(?: in [0-9.]+s)?$"
)
_REQUIRED_CASES = {
    ("test_ci_contract", "test_scoped_ci_pipeline_contract"),
    (
        "test_ci_trust_boundary",
        "test_candidate_contract_implementation_matches_trusted_base",
    ),
    (
        "test_ci_trust_boundary",
        "test_isolated_pip_ignores_repository_package",
    ),
    (
        "test_task_event_liveness",
        "test_liveness_audit_cli_returns_nonzero_for_limbo",
    ),
}


def _required_path(environment_name: str) -> Path:
    raw = os.environ.get(environment_name)
    if not raw:
        raise RuntimeError(f"{environment_name} is required")
    path = Path(raw)
    if not path.is_file():
        raise RuntimeError(f"{environment_name} is not a file: {path}")
    return path


def _expected_case_count(collection_path: Path) -> int:
    summaries = _COLLECTION_SUMMARY.findall(
        collection_path.read_text(encoding="utf-8")
    )
    if len(summaries) != 1:
        raise RuntimeError(
            "trusted repository-contract collection has no unique summary"
        )
    expected = int(summaries[0])
    if expected == 0:
        raise RuntimeError("trusted repository-contract collection is empty")
    return expected


def verify(report_path: Path, collection_path: Path) -> int:
    expected_count = _expected_case_count(collection_path)
    root = ET.parse(report_path).getroot()
    cases = list(root.iter("testcase"))
    case_keys = [
        (case.attrib.get("classname", ""), case.attrib.get("name", ""))
        for case in cases
    ]
    if len(case_keys) != expected_count:
        raise RuntimeError(
            "trusted repository-contract count is "
            f"{len(case_keys)}, expected collected count {expected_count}"
        )

    unique_keys = set(case_keys)
    if len(unique_keys) != len(case_keys):
        raise RuntimeError("trusted repository-contract report has duplicate nodes")

    missing = sorted(_REQUIRED_CASES - unique_keys)
    if missing:
        raise RuntimeError(
            f"trusted repository-contract nodes are missing: {missing}"
        )

    failed = [
        case.attrib.get("name", "")
        for case in cases
        if any(
            case.find(outcome) is not None
            for outcome in ("failure", "error", "skipped")
        )
    ]
    if failed:
        raise RuntimeError(
            f"trusted repository-contract nodes did not pass: {failed}"
        )
    return len(case_keys)


def main() -> None:
    report = _required_path("HARNESS_CONTRACT_REPORT")
    collection = _required_path("HARNESS_CONTRACT_COLLECTION")
    verified = verify(report, collection)
    print(f"verified {verified} trusted repository-contract nodes")


if __name__ == "__main__":
    main()
