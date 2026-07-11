#!/usr/bin/env python3
"""Synthetic-only tests for the GH1574 local NAP replay tooling."""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import stat
import sys
import tempfile
import unittest
from datetime import date
from pathlib import Path
from unittest import mock


SCRIPT_DIR = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location(
    "evaluate_nap_replay", SCRIPT_DIR / "evaluate_nap_replay.py"
)
assert SPEC is not None and SPEC.loader is not None
NAP = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = NAP
SPEC.loader.exec_module(NAP)

SALT = hashlib.sha256(b"synthetic-only-gh1574-test-salt").digest()
SALT_HEX = SALT.hex()
SINCE = date(2026, 6, 1)
THROUGH = date(2026, 7, 12)


def write_session(
    path: Path,
    observation: str,
    *,
    include_variants: bool = False,
    padding: int = 0,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    records: list[dict] = [
        {"type": "session_meta", "payload": {"id": path.stem}},
        {
            "type": "response_item",
            "payload": {"type": "function_call_output", "output": observation},
        },
    ]
    if include_variants:
        records.extend(
            [
                {
                    "type": "response_item",
                    "payload": {
                        "type": "custom_tool_call_output",
                        "output": "custom",
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "tool_search_output",
                        "execution": "search",
                        "tools": [{"name": "fixture"}],
                    },
                },
                {
                    "type": "event_msg",
                    "payload": {"type": "mcp_tool_call_end", "result": "mcp"},
                },
                {
                    "type": "event_msg",
                    "payload": {
                        "type": "patch_apply_end",
                        "stdout": "out",
                        "stderr": "err",
                        "changes": ["file"],
                    },
                },
            ]
        )
    records.append({"type": "event_msg", "payload": {"message": "x" * padding}})
    path.write_text(
        "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
    )


def build_corpus(root: Path, count: int = 40) -> dict:
    for index in range(count):
        day = 1 + index % 28
        path = root / "2026" / "06" / f"{day:02d}" / f"rollout-{index:02d}.jsonl"
        write_session(path, f"observation-{index}-" + "z" * (index + 1))
    return NAP.build_manifest(root, SALT, count, SINCE, THROUGH)


def evidence_for(manifest: dict, overrides: dict[int, dict] | None = None) -> dict:
    overrides = overrides or {}
    sessions = []
    for index, item in enumerate(manifest["sessions"]):
        session = {
            "session_key": item["session_key"],
            "baseline_tokens": 1_000,
            "compressed_tokens": 500,
            "fallback_raw_tokens": 200,
            "untouched_raw_tokens": 100,
            "nap_checked": 1,
            "nap_failed": 0,
            "baseline_success": True,
            "candidate_success": True,
            "compressor_input_tokens": 100,
            "compressor_output_tokens": 25,
        }
        session.update(overrides.get(index, {}))
        sessions.append(session)
    return {
        "schema_version": 1,
        "manifest_id": manifest["manifest_id"],
        "expected_sessions": len(sessions),
        "pricing": {
            "compressor_input_usd_per_million": 1.0,
            "compressor_output_usd_per_million": 2.0,
            "saved_input_usd_per_million": 5.0,
        },
        "sessions": sessions,
    }


class ManifestTests(unittest.TestCase):
    def test_date_filter_ranks_by_observation_bytes_and_emits_no_identifiers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            write_session(
                root / "2026/05/31/rollout-old.jsonl", "old" + "x" * 1_000
            )
            write_session(root / "2026/06/01/rollout-small.jsonl", "small", padding=5_000)
            write_session(root / "2026/06/02/rollout-large.jsonl", "large" + "y" * 100)
            manifest = NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

            self.assertEqual(manifest["selection"]["candidate_sessions"], 2)
            self.assertEqual(manifest["sessions"][0]["observation_bytes"], 105)
            self.assertRegex(
                manifest["sessions"][0]["source_hmac_sha256"], r"^[0-9a-f]{64}$"
            )
            rendered = json.dumps(manifest, sort_keys=True)
            self.assertNotIn("rollout-large", rendered)
            self.assertNotIn(str(root), rendered)
            self.assertNotIn("large", rendered)
            self.assertEqual(manifest["manifest_id"], NAP._manifest_id(manifest))

    def test_event_and_response_observation_variants_are_counted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            write_session(
                root / "2026/06/01/rollout-variants.jsonl",
                "base",
                include_variants=True,
            )
            manifest = NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)
            self.assertEqual(manifest["sessions"][0]["observation_records"], 5)
            self.assertGreater(manifest["sessions"][0]["observation_bytes"], 20)

    def test_manifest_changes_when_same_path_content_changes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            path = root / "2026/06/01/rollout-same.jsonl"
            write_session(path, "first")
            first = NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)
            write_session(path, "second")
            second = NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)
            self.assertEqual(
                first["sessions"][0]["session_key"], second["sessions"][0]["session_key"]
            )
            self.assertNotEqual(first["manifest_id"], second["manifest_id"])
            self.assertNotEqual(
                first["sessions"][0]["source_hmac_sha256"],
                second["sessions"][0]["source_hmac_sha256"],
            )

    def test_symlink_hardlink_malformed_and_duplicate_keys_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            root.mkdir()
            target = root / "rollout-2026-06-01T00.jsonl"
            write_session(target, "one")
            os.link(target, root / "rollout-2026-06-02T00.jsonl")
            with self.assertRaisesRegex(NAP.ReplayValidationError, "link or size"):
                NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            root.mkdir()
            (root / "rollout-2026-06-01T00.jsonl").write_text(
                '{"type":"x","type":"y"}\n', encoding="utf-8"
            )
            with self.assertRaisesRegex(NAP.ReplayValidationError, "duplicate"):
                NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            root.mkdir()
            target = Path(tmp) / "rollout-2026-06-01T00.jsonl"
            write_session(target, "one")
            os.symlink(target, root / "rollout-2026-06-01T01.jsonl")
            with self.assertRaisesRegex(NAP.ReplayValidationError, "regular file"):
                NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

    def test_invalid_window_salt_limits_and_insufficient_evidence_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            write_session(root / "2026/06/01/one.jsonl", "one")
            with self.assertRaisesRegex(NAP.ReplayValidationError, "after"):
                NAP.build_manifest(root, SALT, 1, THROUGH, SINCE)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "insufficient"):
                NAP.build_manifest(root, SALT, 2, SINCE, THROUGH)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "top_n"):
                NAP.build_manifest(root, SALT, NAP.MAX_TOP_N + 1, SINCE, THROUGH)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "CSPRNG"):
                NAP.build_manifest(root, b"x" * 32, 1, SINCE, THROUGH)

    def test_resource_limits_reject_large_sessions_lines_corpora_and_integers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            path = root / "2026/06/01/huge.jsonl"
            path.parent.mkdir(parents=True)
            with path.open("wb") as handle:
                handle.truncate(NAP.MAX_SESSION_BYTES + 1)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "link or size"):
                NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            write_session(root / "2026/06/01/line.jsonl", "x" * 64)
            with mock.patch.object(NAP, "MAX_JSONL_LINE_BYTES", 32):
                with self.assertRaisesRegex(NAP.ReplayValidationError, "invalid-size"):
                    NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            write_session(root / "2026/06/01/total.jsonl", "value")
            with mock.patch.object(NAP, "MAX_TOTAL_SCAN_BYTES", 1):
                with self.assertRaisesRegex(NAP.ReplayValidationError, "scan-byte"):
                    NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)

        with self.assertRaisesRegex(NAP.ReplayValidationError, "integer"):
            NAP._json_loads('{"value":10000000000000000}')

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "sessions"
            path = root / "2026/06/01/overflow.jsonl"
            path.parent.mkdir(parents=True)
            path.write_text(
                '{"type":"response_item","payload":{"type":"tool_search_output",'
                '"execution":"search","tools":[{"score":1e10000}]}}\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(NAP.ReplayValidationError, "float"):
                NAP.build_manifest(root, SALT, 1, SINCE, THROUGH)


class EvidenceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.temp = tempfile.TemporaryDirectory()
        cls.manifest = build_corpus(Path(cls.temp.name) / "sessions")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.temp.cleanup()

    def test_metric_math_counts_fallback_as_raw(self) -> None:
        report = NAP.summarize_evidence(self.manifest, evidence_for(self.manifest))
        metrics = report["metrics"]
        self.assertEqual(metrics["effective_candidate_tokens"], 32_000)
        self.assertEqual(metrics["fallback_raw_tokens"], 8_000)
        self.assertEqual(metrics["saved_tokens"], 8_000)
        self.assertAlmostEqual(metrics["effective_token_saving"], 0.20)
        self.assertEqual(report["manifest_id"], self.manifest["manifest_id"])
        self.assertTrue(report["overall_pass"])

    def test_strict_mismatch_and_cost_thresholds_fail(self) -> None:
        mismatch = evidence_for(
            self.manifest,
            {0: {"nap_failed": 1}, 1: {"nap_failed": 1}},
        )
        mismatch_report = NAP.summarize_evidence(self.manifest, mismatch)
        self.assertAlmostEqual(mismatch_report["metrics"]["nap_mismatch_rate"], 0.05)
        self.assertFalse(mismatch_report["decisions"]["nap_mismatch"])

        exact_cost = evidence_for(self.manifest)
        exact_cost["pricing"] = {
            "compressor_input_usd_per_million": 1.0,
            "compressor_output_usd_per_million": 4.0,
            "saved_input_usd_per_million": 5.0,
        }
        cost_report = NAP.summarize_evidence(self.manifest, exact_cost)
        self.assertAlmostEqual(
            cost_report["metrics"]["compression_cost_saved_value_ratio"], 0.20
        )
        self.assertFalse(cost_report["decisions"]["cost_ratio"])

    def test_paired_regression_fails_non_regression_parity(self) -> None:
        evidence = evidence_for(
            self.manifest,
            {
                0: {"baseline_success": True, "candidate_success": False},
                1: {"baseline_success": False, "candidate_success": True},
            },
        )
        report = NAP.summarize_evidence(self.manifest, evidence)
        self.assertEqual(report["metrics"]["success_rate_delta"], 0.0)
        self.assertEqual(report["metrics"]["paired_success_regressions"], 1)
        self.assertFalse(report["decisions"]["success_parity"])

    def test_zero_or_negative_savings_and_zero_nap_are_reported_as_failures(self) -> None:
        no_savings = evidence_for(self.manifest)
        for session in no_savings["sessions"]:
            session.update(
                compressed_tokens=900,
                fallback_raw_tokens=200,
                untouched_raw_tokens=100,
                nap_checked=0,
            )
        report = NAP.summarize_evidence(self.manifest, no_savings)
        self.assertLess(report["metrics"]["effective_token_saving"], 0)
        self.assertIsNone(report["metrics"]["nap_mismatch_rate"])
        self.assertIsNone(report["metrics"]["compression_cost_saved_value_ratio"])
        self.assertFalse(report["overall_pass"])

    def test_manifest_binding_fixed_40_and_accounting_are_enforced(self) -> None:
        evidence = evidence_for(self.manifest)
        evidence["manifest_id"] = "0" * 64
        with self.assertRaisesRegex(NAP.ReplayValidationError, "manifest_id"):
            NAP.summarize_evidence(self.manifest, evidence)

        small = dict(self.manifest)
        small["selection"] = dict(
            self.manifest["selection"], expected_sessions=39, selected_sessions=39
        )
        small["sessions"] = self.manifest["sessions"][:39]
        small["aggregate"] = {
            field: sum(item[field] for item in small["sessions"])
            for field in NAP.AGGREGATE_KEYS
        }
        small["manifest_id"] = NAP._manifest_id(small)
        with self.assertRaisesRegex(NAP.ReplayValidationError, "40 sessions"):
            NAP.summarize_evidence(small, evidence_for(small))

        excessive = evidence_for(self.manifest)
        excessive["sessions"][0]["nap_checked"] = 2
        with self.assertRaisesRegex(NAP.ReplayValidationError, "observation records"):
            NAP.summarize_evidence(self.manifest, excessive)

    def test_incomplete_unknown_or_unsafe_evidence_is_rejected(self) -> None:
        evidence = evidence_for(self.manifest)
        evidence["sessions"].pop()
        with self.assertRaisesRegex(NAP.ReplayValidationError, "exactly"):
            NAP.summarize_evidence(self.manifest, evidence)

        evidence = evidence_for(self.manifest)
        unsafe_key = "/private/source/path"
        evidence["sessions"][0][unsafe_key] = True
        with self.assertRaisesRegex(
            NAP.ReplayValidationError, "schema mismatch"
        ) as captured:
            NAP.summarize_evidence(self.manifest, evidence)
        self.assertNotIn(unsafe_key, str(captured.exception))

        with self.assertRaisesRegex(NAP.ReplayValidationError, "secret-like"):
            NAP._assert_no_leaks({"value": "github_pat_private"})


class OutputAndCliTests(unittest.TestCase):
    def test_secure_output_requires_private_parent_and_refuses_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            private = Path(tmp) / "private"
            private.mkdir(mode=0o700)
            output = private / "report.json"
            NAP.secure_write_json(output, {"safe": True})
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "write secure output"):
                NAP.secure_write_json(output, {"safe": False})

            public = Path(tmp) / "public"
            public.mkdir(mode=0o755)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "owner-only"):
                NAP.secure_write_json(public / "report.json", {"safe": True})

            real = Path(tmp) / "real"
            real.mkdir(mode=0o700)
            linked = Path(tmp) / "linked"
            os.symlink(real, linked)
            with self.assertRaisesRegex(NAP.ReplayValidationError, "unsafe"):
                NAP.secure_write_json(linked / "report.json", {"safe": True})

    def test_hex_salt_and_duplicate_json_validation(self) -> None:
        with mock.patch.dict(os.environ, {NAP.DEFAULT_SALT_ENV: SALT_HEX}, clear=False):
            self.assertEqual(NAP._load_salt(NAP.DEFAULT_SALT_ENV), SALT)
        with mock.patch.dict(os.environ, {NAP.DEFAULT_SALT_ENV: "not-hex"}, clear=False):
            with self.assertRaisesRegex(NAP.ReplayValidationError, "hexadecimal"):
                NAP._load_salt(NAP.DEFAULT_SALT_ENV)
        with self.assertRaisesRegex(NAP.ReplayValidationError, "duplicate"):
            NAP._json_loads('{"a":1,"a":2}')

    def test_cli_build_and_summarize_returns_zero_or_three(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sessions_root = root / "sessions"
            manifest_value = build_corpus(sessions_root)
            private = root / "private"
            private.mkdir(mode=0o700)
            built_path = private / "built.json"
            with mock.patch.dict(os.environ, {NAP.DEFAULT_SALT_ENV: SALT_HEX}, clear=False):
                result = NAP.main(
                    [
                        "build-manifest",
                        "--sessions-root",
                        str(sessions_root),
                        "--output",
                        str(built_path),
                        "--since-date",
                        "2026-06-01",
                        "--through-date",
                        "2026-07-12",
                    ]
                )
            self.assertEqual(result, 0)
            built = json.loads(built_path.read_text())
            self.assertEqual(built["manifest_id"], manifest_value["manifest_id"])

            manifest_path = private / "manifest-input.json"
            evidence_path = private / "evidence-input.json"
            manifest_path.write_text(json.dumps(built), encoding="utf-8")
            evidence_path.write_text(json.dumps(evidence_for(built)), encoding="utf-8")
            report_path = private / "pass-report.json"
            self.assertEqual(
                NAP.main(
                    [
                        "summarize-evidence",
                        "--manifest",
                        str(manifest_path),
                        "--evidence",
                        str(evidence_path),
                        "--output",
                        str(report_path),
                    ]
                ),
                0,
            )

            failing = evidence_for(built)
            failing["sessions"][0]["candidate_success"] = False
            failing_path = private / "failing-input.json"
            failing_path.write_text(json.dumps(failing), encoding="utf-8")
            fail_report = private / "fail-report.json"
            self.assertEqual(
                NAP.main(
                    [
                        "summarize-evidence",
                        "--manifest",
                        str(manifest_path),
                        "--evidence",
                        str(failing_path),
                        "--output",
                        str(fail_report),
                    ]
                ),
                3,
            )
            self.assertFalse(json.loads(fail_report.read_text())["overall_pass"])

    def test_cli_validation_error_is_sanitized(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            result = NAP.main(
                [
                    "build-manifest",
                    "--sessions-root",
                    "/missing/private/path",
                    "--output",
                    "/missing/output.json",
                ]
            )
        self.assertEqual(result, 2)
        self.assertIn("error:", stderr.getvalue())
        self.assertNotIn("/missing/private/path", stderr.getvalue())

    def test_cli_intermediate_symlink_loop_is_sanitized(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = root / "first"
            second = root / "second"
            os.symlink("second", first)
            os.symlink("first", second)
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                result = NAP.main(
                    [
                        "summarize-evidence",
                        "--manifest",
                        str(first / "nested" / "manifest.json"),
                        "--evidence",
                        str(first / "nested" / "evidence.json"),
                        "--output",
                        str(root / "report.json"),
                    ]
                )

        self.assertEqual(result, 2)
        self.assertIn("error:", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())
        self.assertNotIn(str(root), stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
