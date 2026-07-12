#!/usr/bin/env python3
"""Build a private NAP replay manifest and summarize GH1574 gate evidence.

The tool has no network or subprocess client. ``build-manifest`` reads local
Codex JSONL and writes pseudonymous metadata only. ``summarize-evidence``
accepts already-produced A/B measurements and writes an aggregate-only report.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import math
import os
import re
import stat
import sys
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any

from nap_replay_safety import (
    MAX_JSONL_LINE_BYTES,
    MAX_METRIC_VALUE,
    MAX_RECORDS_PER_SESSION,
    ReplayValidationError,
    _assert_no_leaks,
    _json_loads,
    _load_salt,
    _strict_json_load,
    _validate_salt,
    secure_write_json,
)


SCHEMA_VERSION = 1
PHASE1_EXPECTED_SESSIONS = 40
DEFAULT_SALT_ENV = "HARNESS_NAP_REPLAY_SALT"
MAX_TOP_N = 1_000
MAX_CANDIDATE_FILES = 100_000
MAX_SESSION_BYTES = 1024 * 1024 * 1024
MAX_TOTAL_SCAN_BYTES = 16 * 1024 * 1024 * 1024

SESSION_KEY_RE = re.compile(r"session_[0-9a-f]{32}\Z")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
ROLLOUT_DATE_RE = re.compile(r"rollout-(\d{4}-\d{2}-\d{2})T")
OBSERVATION_FIELDS = {
    "function_call_output": ("output",),
    "custom_tool_call_output": ("output",),
    "tool_call_output": ("output",),
    "tool_output": ("output", "content", "result"),
    "tool_search_output": ("execution", "tools"),
    "mcp_tool_call_end": ("result",),
    "patch_apply_end": ("stdout", "stderr", "changes"),
}

MANIFEST_KEYS = {
    "schema_version",
    "manifest_id",
    "corpus_kind",
    "selection",
    "sessions",
    "aggregate",
}
SELECTION_KEYS = {
    "candidate_sessions",
    "expected_sessions",
    "selected_sessions",
    "since_date",
    "through_date",
    "order",
}
MANIFEST_SESSION_KEYS = {
    "session_key",
    "source_hmac_sha256",
    "bytes",
    "records",
    "observation_records",
    "observation_bytes",
}
AGGREGATE_KEYS = {"bytes", "records", "observation_records", "observation_bytes"}
EVIDENCE_KEYS = {
    "schema_version",
    "manifest_id",
    "expected_sessions",
    "pricing",
    "sessions",
}
PRICING_KEYS = {
    "compressor_input_usd_per_million",
    "compressor_output_usd_per_million",
    "saved_input_usd_per_million",
}
EVIDENCE_SESSION_KEYS = {
    "session_key",
    "baseline_tokens",
    "compressed_tokens",
    "fallback_raw_tokens",
    "untouched_raw_tokens",
    "nap_checked",
    "nap_failed",
    "baseline_success",
    "candidate_success",
    "compressor_input_tokens",
    "compressor_output_tokens",
}


@dataclass(frozen=True)
class Candidate:
    path: Path
    relative_path: str
    size: int
    session_key: str
    device: int
    inode: int
    modified_ns: int
    changed_ns: int


def _require_object(value: Any, label: str, keys: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ReplayValidationError(f"{label} must be an object")
    actual = set(value)
    if actual != keys:
        raise ReplayValidationError(
            f"{label} schema mismatch; missing_count={len(keys - actual)}, "
            f"extra_count={len(actual - keys)}"
        )
    return value


def _require_int(value: Any, label: str, *, positive: bool = False) -> int:
    minimum = 1 if positive else 0
    if type(value) is not int or not minimum <= value <= MAX_METRIC_VALUE:
        qualifier = "positive" if positive else "non-negative"
        raise ReplayValidationError(f"{label} must be a bounded {qualifier} integer")
    return value


def _require_number(value: Any, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ReplayValidationError(f"{label} must be a non-negative number")
    result = float(value)
    if not math.isfinite(result) or not 0 <= result <= MAX_METRIC_VALUE:
        raise ReplayValidationError(f"{label} must be a bounded non-negative number")
    return result


def _require_bool(value: Any, label: str) -> bool:
    if type(value) is not bool:
        raise ReplayValidationError(f"{label} must be a boolean")
    return value


def _require_session_key(value: Any, label: str) -> str:
    if not isinstance(value, str) or SESSION_KEY_RE.fullmatch(value) is None:
        raise ReplayValidationError(f"{label} must be a pseudonymous session key")
    return value


def _pseudonym(relative_path: str, salt: bytes) -> str:
    message = b"harness.nap-replay.session.v1\0" + relative_path.encode("utf-8")
    digest = hmac.new(salt, message, hashlib.sha256).hexdigest()
    return f"session_{digest[:32]}"


def _parse_date(raw: str) -> date:
    try:
        return date.fromisoformat(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("date must use YYYY-MM-DD") from exc


def _session_date(relative_path: str) -> date:
    parts = Path(relative_path).parts
    if len(parts) >= 4:
        try:
            parsed = date(int(parts[-4]), int(parts[-3]), int(parts[-2]))
        except (TypeError, ValueError):
            parsed = None
        if parsed is not None:
            return parsed
    match = ROLLOUT_DATE_RE.search(Path(relative_path).name)
    if match is None:
        raise ReplayValidationError("session candidate has no parseable rollout date")
    try:
        return date.fromisoformat(match.group(1))
    except ValueError as exc:
        raise ReplayValidationError("session candidate has an invalid rollout date") from exc


def _walk_jsonl(
    root: Path,
    salt: bytes,
    since_date: date | None,
    through_date: date | None,
) -> list[Candidate]:
    if root.is_symlink() or not root.is_dir():
        raise ReplayValidationError("sessions root must be a real directory")
    if since_date and through_date and since_date > through_date:
        raise ReplayValidationError("since_date must not be after through_date")
    root = root.resolve()
    candidates: list[Candidate] = []
    seen_files: set[tuple[int, int]] = set()
    visited_entries = 0

    def walk_error(_: OSError) -> None:
        raise ReplayValidationError("failed to traverse the sessions root")

    for current, directories, files in os.walk(
        root, followlinks=False, onerror=walk_error
    ):
        current_path = Path(current)
        visited_entries += len(directories) + len(files)
        if visited_entries > MAX_CANDIDATE_FILES:
            raise ReplayValidationError("sessions root exceeds the visited-entry limit")
        if any((current_path / item).is_symlink() for item in directories):
            raise ReplayValidationError("sessions root contains a directory symlink")
        for filename in files:
            if not filename.endswith(".jsonl"):
                continue
            path = current_path / filename
            try:
                info = path.lstat()
            except OSError as exc:
                raise ReplayValidationError("failed to stat a session file") from exc
            if path.is_symlink() or not stat.S_ISREG(info.st_mode):
                raise ReplayValidationError("session candidate must be a regular file")
            if info.st_nlink != 1 or info.st_size > MAX_SESSION_BYTES:
                raise ReplayValidationError("session candidate violates link or size limits")
            relative = path.relative_to(root).as_posix()
            observed_date = _session_date(relative)
            if since_date and observed_date < since_date:
                continue
            if through_date and observed_date > through_date:
                continue
            identity = (info.st_dev, info.st_ino)
            if identity in seen_files:
                raise ReplayValidationError("sessions root contains duplicate hard-linked files")
            seen_files.add(identity)
            candidates.append(
                Candidate(
                    path,
                    relative,
                    info.st_size,
                    _pseudonym(relative, salt),
                    info.st_dev,
                    info.st_ino,
                    info.st_mtime_ns,
                    info.st_ctime_ns,
                )
            )
    return candidates


def _content_bytes(value: Any) -> int:
    if isinstance(value, str):
        return len(value.encode("utf-8"))
    if isinstance(value, (list, dict, bool, int, float)):
        try:
            encoded = json.dumps(
                value,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=False,
                allow_nan=False,
            )
        except (TypeError, ValueError, OverflowError) as exc:
            raise ReplayValidationError(
                "observation content has an unsupported JSON type"
            ) from exc
        return len(encoded.encode("utf-8"))
    if value is None:
        return 0
    raise ReplayValidationError("observation content has an unsupported JSON type")


def _observation_bytes(record: dict[str, Any]) -> int | None:
    outer_type = record.get("type")
    if not isinstance(outer_type, str) or not outer_type:
        raise ReplayValidationError("every JSONL record must have a string type")
    observation = record
    observation_type = outer_type
    if outer_type in {"response_item", "event_msg"}:
        observation = record.get("payload")
        if not isinstance(observation, dict):
            raise ReplayValidationError(f"{outer_type} payload must be an object")
        observation_type = observation.get("type")
    fields = OBSERVATION_FIELDS.get(observation_type)
    if fields is None:
        return None
    present = [observation[field] for field in fields if field in observation]
    if not present:
        raise ReplayValidationError("observation record has no supported content field")
    return sum(_content_bytes(value) for value in present)


def _scan_candidate(candidate: Candidate, salt: bytes) -> dict[str, Any]:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(candidate.path, flags)
    except OSError as exc:
        raise ReplayValidationError("failed to open a selected session safely") from exc
    records = observation_records = observation_bytes = 0
    digest_key = hmac.new(
        salt, b"harness.nap-replay.source-content.v1", hashlib.sha256
    ).digest()
    digest = hmac.new(digest_key, digestmod=hashlib.sha256)
    try:
        with os.fdopen(descriptor, "rb") as handle:
            before = os.fstat(handle.fileno())
            expected = (
                candidate.device,
                candidate.inode,
                candidate.size,
                candidate.modified_ns,
                candidate.changed_ns,
            )
            observed = (
                before.st_dev,
                before.st_ino,
                before.st_size,
                before.st_mtime_ns,
                before.st_ctime_ns,
            )
            if observed != expected:
                raise ReplayValidationError("session changed after candidate discovery")
            while True:
                raw_line = handle.readline(MAX_JSONL_LINE_BYTES + 1)
                if not raw_line:
                    break
                records += 1
                if records > MAX_RECORDS_PER_SESSION:
                    raise ReplayValidationError("session exceeds the record-count limit")
                if not raw_line.strip() or len(raw_line) > MAX_JSONL_LINE_BYTES:
                    raise ReplayValidationError("session contains an invalid-size record")
                digest.update(raw_line)
                try:
                    record = _json_loads(raw_line.decode("utf-8"))
                except (UnicodeError, json.JSONDecodeError, RecursionError) as exc:
                    raise ReplayValidationError("session contains invalid JSONL") from exc
                if not isinstance(record, dict):
                    raise ReplayValidationError("every JSONL record must be an object")
                size = _observation_bytes(record)
                if size is not None:
                    observation_records += 1
                    observation_bytes += size
            after = os.fstat(handle.fileno())
            stable = (
                after.st_dev,
                after.st_ino,
                after.st_size,
                after.st_mtime_ns,
                after.st_ctime_ns,
            )
            if stable != expected:
                raise ReplayValidationError("session changed while it was scanned")
    except ReplayValidationError:
        raise
    except OSError as exc:
        raise ReplayValidationError("failed while scanning a selected session") from exc
    return {
        "session_key": candidate.session_key,
        "source_hmac_sha256": digest.hexdigest(),
        "bytes": candidate.size,
        "records": records,
        "observation_records": observation_records,
        "observation_bytes": observation_bytes,
    }


def _manifest_id(manifest: dict[str, Any]) -> str:
    body = {key: value for key, value in manifest.items() if key != "manifest_id"}
    encoded = json.dumps(body, sort_keys=True, separators=(",", ":"), allow_nan=False)
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def build_manifest(
    sessions_root: Path,
    salt: bytes,
    top_n: int = PHASE1_EXPECTED_SESSIONS,
    since_date: date | None = None,
    through_date: date | None = None,
) -> dict[str, Any]:
    _validate_salt(salt)
    if not 1 <= top_n <= MAX_TOP_N:
        raise ReplayValidationError(f"top_n must be between 1 and {MAX_TOP_N}")
    candidates = _walk_jsonl(sessions_root, salt, since_date, through_date)
    if len(candidates) < top_n:
        raise ReplayValidationError(
            f"insufficient session evidence: expected {top_n}, found {len(candidates)}"
        )
    if sum(candidate.size for candidate in candidates) > MAX_TOTAL_SCAN_BYTES:
        raise ReplayValidationError("filtered corpus exceeds the total scan-byte limit")
    scanned = [_scan_candidate(candidate, salt) for candidate in candidates]
    scanned.sort(
        key=lambda item: (
            -item["observation_bytes"],
            -item["bytes"],
            item["session_key"],
        )
    )
    sessions = scanned[:top_n]
    aggregate = {
        field: sum(session[field] for session in sessions) for field in AGGREGATE_KEYS
    }
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "manifest_id": "",
        "corpus_kind": "codex_jsonl",
        "selection": {
            "candidate_sessions": len(candidates),
            "expected_sessions": top_n,
            "selected_sessions": len(sessions),
            "since_date": since_date.isoformat() if since_date else None,
            "through_date": through_date.isoformat() if through_date else None,
            "order": "observation_bytes_desc_then_total_bytes_then_session_key",
        },
        "sessions": sessions,
        "aggregate": aggregate,
    }
    manifest["manifest_id"] = _manifest_id(manifest)
    forbidden = [part for item in candidates for part in (item.relative_path, item.path.name)]
    _validate_manifest(manifest)
    _assert_no_leaks(manifest, forbidden)
    return manifest


def _validate_manifest(value: Any) -> dict[str, Any]:
    manifest = _require_object(value, "manifest", MANIFEST_KEYS)
    if manifest["schema_version"] != SCHEMA_VERSION:
        raise ReplayValidationError("unsupported manifest schema_version")
    if manifest["corpus_kind"] != "codex_jsonl":
        raise ReplayValidationError("manifest corpus_kind must be codex_jsonl")
    if not isinstance(manifest["manifest_id"], str) or not SHA256_RE.fullmatch(
        manifest["manifest_id"]
    ):
        raise ReplayValidationError("manifest_id must be a SHA-256 digest")
    if not hmac.compare_digest(manifest["manifest_id"], _manifest_id(manifest)):
        raise ReplayValidationError("manifest_id does not match the manifest content")
    selection = _require_object(manifest["selection"], "manifest.selection", SELECTION_KEYS)
    candidates = _require_int(
        selection["candidate_sessions"], "manifest candidate_sessions", positive=True
    )
    expected = _require_int(
        selection["expected_sessions"], "manifest expected_sessions", positive=True
    )
    selected = _require_int(
        selection["selected_sessions"], "manifest selected_sessions", positive=True
    )
    if candidates < expected or selected != expected:
        raise ReplayValidationError("manifest lacks the expected session evidence")
    if selection["order"] != "observation_bytes_desc_then_total_bytes_then_session_key":
        raise ReplayValidationError("manifest selection order is unsupported")
    for field in ("since_date", "through_date"):
        raw = selection[field]
        if raw is not None:
            if not isinstance(raw, str):
                raise ReplayValidationError(f"manifest {field} must be an ISO date or null")
            try:
                date.fromisoformat(raw)
            except ValueError as exc:
                raise ReplayValidationError(f"manifest {field} is invalid") from exc
    sessions = manifest["sessions"]
    if not isinstance(sessions, list) or len(sessions) != expected:
        raise ReplayValidationError("manifest session count does not match expected_sessions")
    keys: set[str] = set()
    totals = {field: 0 for field in AGGREGATE_KEYS}
    previous_order: tuple[int, int, str] | None = None
    for index, raw_session in enumerate(sessions):
        session = _require_object(
            raw_session, f"manifest.sessions[{index}]", MANIFEST_SESSION_KEYS
        )
        key = _require_session_key(session["session_key"], "manifest session_key")
        if key in keys:
            raise ReplayValidationError("manifest contains duplicate session keys")
        keys.add(key)
        if not isinstance(session["source_hmac_sha256"], str) or not SHA256_RE.fullmatch(
            session["source_hmac_sha256"]
        ):
            raise ReplayValidationError("manifest source_hmac_sha256 is invalid")
        for field in AGGREGATE_KEYS:
            totals[field] += _require_int(
                session[field], f"manifest.sessions[{index}].{field}"
            )
        order = (-session["observation_bytes"], -session["bytes"], key)
        if previous_order is not None and order < previous_order:
            raise ReplayValidationError("manifest sessions are not in canonical order")
        previous_order = order
    aggregate = _require_object(manifest["aggregate"], "manifest.aggregate", AGGREGATE_KEYS)
    for field, total in totals.items():
        if _require_int(aggregate[field], f"manifest.aggregate.{field}") != total:
            raise ReplayValidationError(f"manifest aggregate {field} does not match sessions")
    _assert_no_leaks(manifest)
    return manifest


def _validate_evidence(value: Any, manifest: dict[str, Any]) -> dict[str, Any]:
    evidence = _require_object(value, "evidence", EVIDENCE_KEYS)
    if evidence["schema_version"] != SCHEMA_VERSION:
        raise ReplayValidationError("unsupported evidence schema_version")
    if not isinstance(evidence["manifest_id"], str) or not hmac.compare_digest(
        evidence["manifest_id"], manifest["manifest_id"]
    ):
        raise ReplayValidationError("evidence manifest_id does not match the manifest")
    expected = _require_int(
        evidence["expected_sessions"], "evidence expected_sessions", positive=True
    )
    if expected != manifest["selection"]["expected_sessions"]:
        raise ReplayValidationError("evidence expected_sessions does not match manifest")
    pricing = _require_object(evidence["pricing"], "evidence.pricing", PRICING_KEYS)
    for field in PRICING_KEYS:
        _require_number(pricing[field], f"evidence.pricing.{field}")
    if float(pricing["saved_input_usd_per_million"]) <= 0:
        raise ReplayValidationError("saved_input_usd_per_million must be positive")
    sessions = evidence["sessions"]
    if not isinstance(sessions, list) or len(sessions) != expected:
        raise ReplayValidationError("evidence does not contain exactly expected_sessions")
    manifest_sessions = {item["session_key"]: item for item in manifest["sessions"]}
    evidence_keys: set[str] = set()
    for index, raw_session in enumerate(sessions):
        session = _require_object(
            raw_session, f"evidence.sessions[{index}]", EVIDENCE_SESSION_KEYS
        )
        key = _require_session_key(session["session_key"], "evidence session_key")
        if key in evidence_keys or key not in manifest_sessions:
            raise ReplayValidationError("evidence contains duplicate or unknown session keys")
        evidence_keys.add(key)
        for field in EVIDENCE_SESSION_KEYS - {
            "session_key",
            "baseline_success",
            "candidate_success",
        }:
            _require_int(session[field], f"evidence.sessions[{index}].{field}")
        _require_bool(session["baseline_success"], "evidence baseline_success")
        _require_bool(session["candidate_success"], "evidence candidate_success")
        if session["baseline_tokens"] == 0:
            raise ReplayValidationError("each baseline token count must be positive")
        effective = sum(
            session[field]
            for field in ("compressed_tokens", "fallback_raw_tokens", "untouched_raw_tokens")
        )
        if effective == 0:
            raise ReplayValidationError("each effective candidate token count must be positive")
        if session["nap_failed"] > session["nap_checked"]:
            raise ReplayValidationError("nap_failed cannot exceed nap_checked")
        if session["nap_checked"] > manifest_sessions[key]["observation_records"]:
            raise ReplayValidationError("nap_checked exceeds manifest observation records")
    if evidence_keys != set(manifest_sessions):
        raise ReplayValidationError("evidence session keys do not match the manifest")
    _assert_no_leaks(evidence)
    return evidence


def summarize_evidence(manifest_value: Any, evidence_value: Any) -> dict[str, Any]:
    manifest = _validate_manifest(manifest_value)
    if manifest["selection"]["expected_sessions"] != PHASE1_EXPECTED_SESSIONS:
        raise ReplayValidationError("GH1574 Phase 1 requires exactly 40 sessions")
    if not manifest["selection"]["since_date"] or not manifest["selection"]["through_date"]:
        raise ReplayValidationError("GH1574 Phase 1 requires an explicit corpus date window")
    evidence = _validate_evidence(evidence_value, manifest)
    sessions = evidence["sessions"]
    expected = evidence["expected_sessions"]

    baseline_tokens = sum(item["baseline_tokens"] for item in sessions)
    compressed_tokens = sum(item["compressed_tokens"] for item in sessions)
    fallback_raw_tokens = sum(item["fallback_raw_tokens"] for item in sessions)
    untouched_raw_tokens = sum(item["untouched_raw_tokens"] for item in sessions)
    effective_tokens = compressed_tokens + fallback_raw_tokens + untouched_raw_tokens
    saved_tokens = baseline_tokens - effective_tokens
    token_saving = saved_tokens / baseline_tokens

    nap_checked = sum(item["nap_checked"] for item in sessions)
    nap_failed = sum(item["nap_failed"] for item in sessions)
    nap_mismatch = nap_failed / nap_checked if nap_checked else None

    baseline_successes = sum(item["baseline_success"] for item in sessions)
    candidate_successes = sum(item["candidate_success"] for item in sessions)
    regressions = sum(
        item["baseline_success"] and not item["candidate_success"] for item in sessions
    )
    improvements = sum(
        not item["baseline_success"] and item["candidate_success"] for item in sessions
    )
    baseline_success_rate = baseline_successes / expected
    candidate_success_rate = candidate_successes / expected

    compressor_input_tokens = sum(item["compressor_input_tokens"] for item in sessions)
    compressor_output_tokens = sum(item["compressor_output_tokens"] for item in sessions)
    pricing = evidence["pricing"]
    compression_cost = (
        compressor_input_tokens * float(pricing["compressor_input_usd_per_million"])
        + compressor_output_tokens * float(pricing["compressor_output_usd_per_million"])
    ) / 1_000_000
    saved_value = saved_tokens * float(pricing["saved_input_usd_per_million"]) / 1_000_000
    cost_ratio = compression_cost / saved_value if saved_value > 0 else None

    decisions = {
        "token_saving": token_saving >= 0.20,
        "nap_mismatch": nap_mismatch is not None and nap_mismatch < 0.05,
        "success_parity": regressions == 0,
        "cost_ratio": cost_ratio is not None and cost_ratio < 0.20,
    }
    report = {
        "schema_version": SCHEMA_VERSION,
        "manifest_id": manifest["manifest_id"],
        "gate": "gh1574_phase1",
        "corpus_window": {
            "since_date": manifest["selection"]["since_date"],
            "through_date": manifest["selection"]["through_date"],
        },
        "expected_sessions": expected,
        "observed_sessions": len(sessions),
        "denominators": {
            "baseline_tokens": baseline_tokens,
            "nap_checked": nap_checked,
            "success_sessions": expected,
            "saved_token_value_usd": saved_value,
        },
        "metrics": {
            "effective_candidate_tokens": effective_tokens,
            "compressed_tokens": compressed_tokens,
            "fallback_raw_tokens": fallback_raw_tokens,
            "untouched_raw_tokens": untouched_raw_tokens,
            "saved_tokens": saved_tokens,
            "effective_token_saving": token_saving,
            "nap_failed": nap_failed,
            "nap_mismatch_rate": nap_mismatch,
            "baseline_successes": baseline_successes,
            "candidate_successes": candidate_successes,
            "baseline_success_rate": baseline_success_rate,
            "candidate_success_rate": candidate_success_rate,
            "success_rate_delta": candidate_success_rate - baseline_success_rate,
            "paired_success_regressions": regressions,
            "paired_success_improvements": improvements,
            "compressor_input_tokens": compressor_input_tokens,
            "compressor_output_tokens": compressor_output_tokens,
            "compression_cost_usd": compression_cost,
            "compression_cost_saved_value_ratio": cost_ratio,
        },
        "pricing": dict(pricing),
        "thresholds": {
            "effective_token_saving_min_inclusive": 0.20,
            "nap_mismatch_max_exclusive": 0.05,
            "paired_success_regressions_max_inclusive": 0,
            "compression_cost_saved_value_ratio_max_exclusive": 0.20,
        },
        "decisions": decisions,
        "overall_pass": all(decisions.values()),
    }
    _assert_no_leaks(report)
    return report


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    manifest = subparsers.add_parser("build-manifest", help="build private corpus metadata")
    manifest.add_argument("--sessions-root", required=True, type=Path)
    manifest.add_argument("--output", required=True, type=Path)
    manifest.add_argument("--top-n", type=int, default=PHASE1_EXPECTED_SESSIONS)
    manifest.add_argument("--since-date", type=_parse_date)
    manifest.add_argument("--through-date", type=_parse_date)
    manifest.add_argument("--salt-env", default=DEFAULT_SALT_ENV)
    summarize = subparsers.add_parser(
        "summarize-evidence", help="evaluate aggregate evidence against GH1574 gates"
    )
    summarize.add_argument("--manifest", required=True, type=Path)
    summarize.add_argument("--evidence", required=True, type=Path)
    summarize.add_argument("--output", required=True, type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "build-manifest":
            manifest = build_manifest(
                args.sessions_root,
                _load_salt(args.salt_env),
                args.top_n,
                args.since_date,
                args.through_date,
            )
            secure_write_json(args.output, manifest)
            return 0
        manifest = _strict_json_load(args.manifest)
        evidence = _strict_json_load(args.evidence)
        report = summarize_evidence(manifest, evidence)
        secure_write_json(args.output, report)
        return 0 if report["overall_pass"] else 3
    except ReplayValidationError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except (OSError, UnicodeError, RecursionError, OverflowError, MemoryError, ValueError):
        print("error: unsafe or invalid replay input", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
