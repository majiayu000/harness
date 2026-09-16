#!/usr/bin/env python3
"""Decide whether a live eval report may be proposed as a baseline."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


class EligibilityError(RuntimeError):
    """The candidate is not safe to promote as a reviewed baseline."""


def validate_eligibility(
    report: dict[str, Any], *, baseline_present: bool, comparison_outcome: str
) -> None:
    if report.get("outcome") is not None:
        raise EligibilityError(f"candidate run is incomplete: {report['outcome']}")
    metrics = report.get("metrics")
    if not isinstance(metrics, dict):
        raise EligibilityError("candidate report is missing metrics")
    for field in ("pending_cases", "skipped_cases", "infra_failed_cases"):
        count = metrics.get(field)
        if type(count) is not int or count < 0:
            raise EligibilityError(f"candidate report is missing a valid {field} count")
        if count != 0:
            raise EligibilityError("candidate report contains incomplete infrastructure cases")
    cases = report.get("cases")
    if not isinstance(cases, list) or not cases:
        raise EligibilityError("candidate report is missing cases")
    for case in cases:
        if not isinstance(case, dict) or case.get("status") not in ("passed", "failed"):
            raise EligibilityError("candidate report contains a non-baseline-eligible case")
        if case.get("outcome") is not None:
            raise EligibilityError("candidate report contains an incomplete case outcome")
    if baseline_present and comparison_outcome != "success":
        raise EligibilityError("candidate did not pass comparison with the reviewed baseline")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--baseline-present", required=True, choices=("true", "false"))
    parser.add_argument("--comparison-outcome", required=True)
    args = parser.parse_args()
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        if not isinstance(report, dict):
            raise EligibilityError("candidate report must be a JSON object")
        validate_eligibility(
            report,
            baseline_present=args.baseline_present == "true",
            comparison_outcome=args.comparison_outcome,
        )
    except (EligibilityError, OSError, json.JSONDecodeError) as error:
        print(f"eval baseline eligibility failed: {error}")
        return 1
    print("eval baseline eligibility passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
