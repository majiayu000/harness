#!/usr/bin/env python3
"""precision-tracker.py — Rule graduation lifecycle manager.

Reads .harness/triage.jsonl, recomputes per-rule precision stats,
evaluates lifecycle transitions, and updates .harness/rule-scorecard.json.

Usage:
    python tools/precision-tracker.py [--report] [--scorecard PATH] [--triage PATH]

Options:
    --report        Print a human-readable monthly precision report to stdout.
    --scorecard     Path to rule-scorecard.json  (default: .harness/rule-scorecard.json)
    --triage        Path to triage.jsonl          (default: .harness/triage.jsonl)
"""

import argparse
import json
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


LIFECYCLE_ORDER = ["experimental", "warn", "error_block", "demoted", "disabled"]

PRECISION_WARN_TO_ERRORBLOCK = 0.90
PRECISION_EXPERIMENTAL_TO_WARN = 0.70
PRECISION_DEMOTION_THRESHOLD = 0.80
MIN_SAMPLES_WARN = 20
MIN_SAMPLES_ERRORBLOCK = 50
FP_FREE_DAYS = 30


def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _parse_dt(s: Optional[str]) -> Optional[datetime]:
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None


def load_triage(path: Path) -> list[dict]:
    if not path.exists():
        return []
    records = []
    for i, line in enumerate(path.read_text().splitlines(), 1):
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError as e:
            print(f"[WARN] triage.jsonl line {i}: {e}", file=sys.stderr)
    return records


def load_scorecard(path: Path) -> dict:
    if not path.exists():
        return {"updated_at": _now_iso(), "rules": {}}
    return json.loads(path.read_text())


def save_scorecard(scorecard: dict, path: Path) -> None:
    scorecard["updated_at"] = _now_iso()
    path.write_text(json.dumps(scorecard, indent=2) + "\n")


def aggregate_triage(records: list[dict]) -> dict[str, dict]:
    """Return per-rule counts: {rule_id: {tp, fp, acceptable, last_fp_at}}."""
    stats: dict[str, dict] = defaultdict(
        lambda: {"tp": 0, "fp": 0, "acceptable": 0, "last_fp_at": None}
    )
    for r in records:
        rule_id = r.get("rule_id", "")
        verdict = r.get("verdict", "")
        ts = r.get("ts")
        if verdict == "tp":
            stats[rule_id]["tp"] += 1
        elif verdict == "fp":
            stats[rule_id]["fp"] += 1
            if ts:
                prev = stats[rule_id]["last_fp_at"]
                if prev is None or ts > prev:
                    stats[rule_id]["last_fp_at"] = ts
        elif verdict == "acceptable":
            stats[rule_id]["acceptable"] += 1
    return dict(stats)


def evaluate_transition(entry: dict, now: datetime) -> bool:
    """Apply lifecycle transition rules in-place. Returns True if stage changed."""
    precision = entry.get("precision")
    samples = entry.get("samples", 0)
    lifecycle = entry.get("lifecycle", "warn")

    if precision is None:
        return False

    if lifecycle == "experimental":
        if precision >= PRECISION_EXPERIMENTAL_TO_WARN and samples >= MIN_SAMPLES_WARN:
            entry["lifecycle"] = "warn"
            entry["promoted_at"] = now.isoformat(timespec="seconds").replace("+00:00", "Z")
            return True

    elif lifecycle == "warn":
        if precision < PRECISION_DEMOTION_THRESHOLD:
            entry["lifecycle"] = "demoted"
            entry["promoted_at"] = now.isoformat(timespec="seconds").replace("+00:00", "Z")
            return True
        if precision >= PRECISION_WARN_TO_ERRORBLOCK and samples >= MIN_SAMPLES_ERRORBLOCK:
            last_fp = _parse_dt(entry.get("last_fp_at"))
            fp_ok = last_fp is None or (now - last_fp).days >= FP_FREE_DAYS
            if fp_ok:
                entry["lifecycle"] = "error_block"
                entry["promoted_at"] = now.isoformat(timespec="seconds").replace("+00:00", "Z")
                return True

    elif lifecycle == "error_block":
        if precision < PRECISION_DEMOTION_THRESHOLD:
            entry["lifecycle"] = "demoted"
            entry["promoted_at"] = now.isoformat(timespec="seconds").replace("+00:00", "Z")
            return True

    elif lifecycle == "demoted":
        entry["lifecycle"] = "disabled"
        entry["promoted_at"] = now.isoformat(timespec="seconds").replace("+00:00", "Z")
        return True

    return False


def update_scorecard(scorecard: dict, agg: dict[str, dict], now: datetime) -> list[str]:
    """Apply triage data to scorecard entries. Returns list of transition messages."""
    transitions = []
    rules = scorecard.setdefault("rules", {})

    for rule_id, counts in agg.items():
        entry = rules.setdefault(rule_id, {
            "lifecycle": "experimental",
            "tp": 0, "fp": 0, "acceptable": 0,
            "samples": 0, "precision": None,
            "promoted_at": None, "last_fp_at": None,
            "notes": "",
        })
        entry["tp"] = counts["tp"]
        entry["fp"] = counts["fp"]
        entry["acceptable"] = counts["acceptable"]
        entry["samples"] = counts["tp"] + counts["fp"] + counts["acceptable"]
        if counts["last_fp_at"]:
            existing = entry.get("last_fp_at")
            if existing is None or counts["last_fp_at"] > existing:
                entry["last_fp_at"] = counts["last_fp_at"]

        denom = entry["tp"] + entry["fp"]
        entry["precision"] = entry["tp"] / denom if denom > 0 else None

        old_lifecycle = entry.get("lifecycle", "experimental")
        changed = evaluate_transition(entry, now)
        if changed:
            transitions.append(
                f"  {rule_id}: {old_lifecycle} → {entry['lifecycle']}"
            )

    return transitions


def severity_confidence_label(entry: dict) -> str:
    """Return a Severity×Confidence matrix label."""
    precision = entry.get("precision")
    samples = entry.get("samples", 0)

    if precision is None or samples == 0:
        return "no-data"

    if precision >= 0.90:
        confidence = "high"
    elif precision >= 0.70:
        confidence = "medium"
    else:
        confidence = "low"

    lifecycle = entry.get("lifecycle", "experimental")
    if lifecycle in ("error_block",):
        severity = "high"
    elif lifecycle == "warn":
        severity = "medium"
    else:
        severity = "low"

    matrix = {
        ("high", "high"): "BLOCK",
        ("high", "medium"): "warn+triage",
        ("high", "low"): "warn",
        ("medium", "high"): "warn",
        ("medium", "medium"): "warn",
        ("medium", "low"): "off",
        ("low", "high"): "warn",
        ("low", "medium"): "off",
        ("low", "low"): "off",
    }
    return matrix.get((severity, confidence), "warn")


def print_report(scorecard: dict) -> None:
    rules = scorecard.get("rules", {})
    updated = scorecard.get("updated_at", "unknown")
    print(f"=== Rule Precision Scorecard — {updated} ===\n")
    print(f"{'Rule':<16} {'Lifecycle':<14} {'Precision':>9} {'Samples':>8} {'TP':>5} {'FP':>5} {'Action':<14}")
    print("-" * 76)

    for rule_id in sorted(rules):
        e = rules[rule_id]
        prec = e.get("precision")
        prec_str = f"{prec * 100:.1f}%" if prec is not None else "   n/a"
        action = severity_confidence_label(e)
        print(
            f"{rule_id:<16} {e.get('lifecycle', 'unknown'):<14} "
            f"{prec_str:>9} {e.get('samples', 0):>8} "
            f"{e.get('tp', 0):>5} {e.get('fp', 0):>5} {action:<14}"
        )

    print()
    low_precision = [
        (rid, e) for rid, e in rules.items()
        if e.get("precision") is not None and e["precision"] < PRECISION_DEMOTION_THRESHOLD
        and e.get("lifecycle") not in ("demoted", "disabled")
    ]
    if low_precision:
        print("⚠  Rules at risk of demotion (precision < 80%):")
        for rid, e in low_precision:
            print(f"   {rid}: {e['precision'] * 100:.1f}% — {e.get('notes', '')[:60]}")
        print()

    near_promotion = [
        (rid, e) for rid, e in rules.items()
        if e.get("lifecycle") == "warn"
        and e.get("precision") is not None
        and e["precision"] >= PRECISION_WARN_TO_ERRORBLOCK
        and e.get("samples", 0) >= MIN_SAMPLES_ERRORBLOCK
    ]
    if near_promotion:
        print("↑  Rules approaching ERROR_BLOCK promotion:")
        for rid, e in near_promotion:
            last_fp = e.get("last_fp_at")
            if last_fp:
                fp_dt = _parse_dt(last_fp)
                days = (datetime.now(timezone.utc) - fp_dt).days if fp_dt else "?"
                print(f"   {rid}: last FP {days} days ago (need {FP_FREE_DAYS})")
            else:
                print(f"   {rid}: no FPs recorded — eligible for promotion")
        print()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="store_true", help="Print human-readable report")
    parser.add_argument("--scorecard", default=".harness/rule-scorecard.json")
    parser.add_argument("--triage", default=".harness/triage.jsonl")
    args = parser.parse_args()

    scorecard_path = Path(args.scorecard)
    triage_path = Path(args.triage)

    records = load_triage(triage_path)
    scorecard = load_scorecard(scorecard_path)
    agg = aggregate_triage(records)
    now = datetime.now(timezone.utc)

    transitions = update_scorecard(scorecard, agg, now)
    save_scorecard(scorecard, scorecard_path)

    if transitions:
        print("Lifecycle transitions:")
        for t in transitions:
            print(t)
        print()

    if args.report:
        print_report(scorecard)

    return 0


if __name__ == "__main__":
    sys.exit(main())
