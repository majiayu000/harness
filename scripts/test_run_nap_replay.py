"""Unit tests for the GH1574 NAP replay executor (no subprocess, no network)."""

from __future__ import annotations

import json
import os
import secrets
import tempfile
import unittest
from pathlib import Path

import run_nap_replay
from evaluate_nap_replay import _validate_evidence, build_manifest
from nap_replay_safety import ReplayValidationError
from run_nap_replay import (
    ChannelError,
    ChannelReply,
    SeededRateSampler,
    SessionReplayer,
    SessionSource,
    _estimate_tokens,
    _locate_sessions,
    _parse_sketch,
    _sketch_agreement,
)

SALT = secrets.token_bytes(32)
AGREEING_SKETCH = '{"intent": "fix test", "target_files": ["a.rs"], "command_class": "cargo_test"}'
DISAGREEING_SKETCH = '{"intent": "open pr", "target_files": ["b.md"], "command_class": "gh_pr"}'


class FakeChannel:
    """Deterministic channel: summaries compress 10x, sketches configurable."""

    def __init__(self, sketch_replies: list[str] | None = None) -> None:
        self.calls: list[str] = []
        self.sketch_replies = sketch_replies or []
        self.sketch_index = 0

    def complete(self, prompt: str) -> ChannelReply:
        self.calls.append(prompt)
        if '"intent"' in prompt:
            if self.sketch_index < len(self.sketch_replies):
                reply = self.sketch_replies[self.sketch_index]
                self.sketch_index += 1
            else:
                reply = AGREEING_SKETCH
            if reply == "ERROR":
                raise ChannelError("injected sketch failure")
            return ChannelReply(reply)
        return ChannelReply("s" * 400)

    def close(self) -> None:
        return None


def _observation_line(payload_bytes: int) -> bytes:
    record = {
        "type": "response_item",
        "payload": {"type": "function_call_output", "output": "x" * payload_bytes},
    }
    return (json.dumps(record) + "\n").encode("utf-8")


def _write_session(root: Path, name: str, observations: int, payload_bytes: int) -> Path:
    day_dir = root / "2026" / "06" / "15"
    day_dir.mkdir(parents=True, exist_ok=True)
    path = day_dir / f"{name}.jsonl"
    meta = json.dumps({"type": "session_meta"}) + "\n"
    with open(path, "wb") as handle:
        handle.write(meta.encode("utf-8"))
        for _ in range(observations):
            handle.write(_observation_line(payload_bytes))
    return path


class SamplerTests(unittest.TestCase):
    def test_matches_rust_fixed_point_semantics(self) -> None:
        sampler = SeededRateSampler(0.5, "session_x")
        selected = sum(sampler.should_verify(seq) for seq in range(1, 1001))
        self.assertTrue(abs(selected - 500) <= 1)
        self.assertFalse(SeededRateSampler(0.0, "s").should_verify(1))
        self.assertTrue(SeededRateSampler(2.0, "s").should_verify(1))
        self.assertFalse(SeededRateSampler(float("nan"), "s").should_verify(1))

    def test_deterministic_per_seed(self) -> None:
        first = [SeededRateSampler(0.1, "session_a").should_verify(n) for n in range(1, 200)]
        second = [SeededRateSampler(0.1, "session_a").should_verify(n) for n in range(1, 200)]
        self.assertEqual(first, second)


class SketchTests(unittest.TestCase):
    def test_parse_accepts_fenced_json(self) -> None:
        sketch = _parse_sketch(f"```json\n{AGREEING_SKETCH}\n```")
        self.assertEqual(sketch["command_class"], "cargo_test")

    def test_parse_rejects_bad_shapes(self) -> None:
        for reply in ("no json", '{"intent": 1, "command_class": "x"}'):
            with self.assertRaises(ReplayValidationError):
                _parse_sketch(reply)

    def test_agreement_is_field_level(self) -> None:
        a = _parse_sketch(AGREEING_SKETCH)
        b = _parse_sketch(AGREEING_SKETCH.replace("cargo_test", "different"))
        self.assertEqual(_sketch_agreement(a, b), 2)


class ReplayerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        os.chmod(self.root, 0o700)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _manifest_and_sources(self, observations: int = 3, payload: int = 4096):
        _write_session(self.root, "rollout-2026-06-15T10-00-00-a", observations, payload)
        _write_session(self.root, "rollout-2026-06-15T11-00-00-b", observations, payload)
        manifest = build_manifest(self.root, SALT, top_n=2)
        sources = _locate_sessions(manifest, self.root, SALT)
        return manifest, sources

    def _replayer(self, channel, rate: float = 1.0) -> SessionReplayer:
        return SessionReplayer(
            channel,
            SALT,
            min_size_bytes=2048,
            nap_sample_rate=rate,
            task_summary="test",
        )

    def test_verified_replay_produces_valid_evidence(self) -> None:
        manifest, sources = self._manifest_and_sources()
        replayer = self._replayer(FakeChannel())
        results = [replayer.replay(source) for source in sources]
        evidence = {
            "schema_version": 1,
            "manifest_id": manifest["manifest_id"],
            "expected_sessions": 2,
            "pricing": {
                "compressor_input_usd_per_million": 0.0,
                "compressor_output_usd_per_million": 0.0,
                "saved_input_usd_per_million": 1.0,
            },
            "sessions": results,
        }
        for session in results:
            self.assertTrue(session["baseline_success"])
            self.assertTrue(session["candidate_success"])
            self.assertGreater(session["baseline_tokens"], session["compressed_tokens"])
            self.assertEqual(session["nap_failed"], 0)
        # summarize_evidence enforces the 40-session Phase 1 shape, so bind
        # the schema by direct validation of the evidence session records.
        self.assertEqual(len(_validate_evidence(evidence, manifest)["sessions"]), 2)

    def test_small_observations_stay_untouched(self) -> None:
        _write_session(self.root, "rollout-2026-06-15T10-00-00-a", 3, 100)
        _write_session(self.root, "rollout-2026-06-15T11-00-00-b", 3, 100)
        manifest = build_manifest(self.root, SALT, top_n=2)
        sources = _locate_sessions(manifest, self.root, SALT)
        channel = FakeChannel()
        result = self._replayer(channel).replay(sources[0])
        self.assertEqual(channel.calls, [])
        self.assertEqual(result["compressed_tokens"], 0)
        self.assertEqual(result["untouched_raw_tokens"], result["baseline_tokens"])

    def test_nap_mismatch_falls_back_to_raw(self) -> None:
        manifest, sources = self._manifest_and_sources(observations=1)
        channel = FakeChannel(sketch_replies=[AGREEING_SKETCH, DISAGREEING_SKETCH])
        result = self._replayer(channel).replay(sources[0])
        self.assertEqual(result["nap_checked"], 1)
        self.assertEqual(result["nap_failed"], 1)
        self.assertEqual(result["compressed_tokens"], 0)
        self.assertGreater(result["fallback_raw_tokens"], 0)

    def test_breaker_trips_and_fails_candidate(self) -> None:
        manifest, sources = self._manifest_and_sources(observations=8)
        replies = [AGREEING_SKETCH, DISAGREEING_SKETCH] * 8
        result = self._replayer(FakeChannel(sketch_replies=replies)).replay(sources[0])
        self.assertGreaterEqual(result["nap_checked"], 5)
        self.assertFalse(result["candidate_success"])
        self.assertGreater(result["untouched_raw_tokens"], 0)

    def test_oversized_observation_skips_model_call(self) -> None:
        manifest, sources = self._manifest_and_sources(observations=1, payload=8192)
        channel = FakeChannel()
        replayer = SessionReplayer(
            channel,
            SALT,
            min_size_bytes=2048,
            nap_sample_rate=1.0,
            task_summary="test",
            max_observation_bytes=4096,
        )
        result = replayer.replay(sources[0])
        self.assertEqual(channel.calls, [])
        self.assertEqual(result["untouched_raw_tokens"], result["baseline_tokens"])

    def test_channel_error_keeps_raw(self) -> None:
        manifest, sources = self._manifest_and_sources(observations=1)
        channel = FakeChannel(sketch_replies=["ERROR"])
        result = self._replayer(channel).replay(sources[0])
        self.assertEqual(result["nap_checked"], 0)
        self.assertEqual(result["compressed_tokens"], 0)
        self.assertEqual(result["untouched_raw_tokens"], result["baseline_tokens"])

    def test_modified_session_fails_hmac_binding(self) -> None:
        manifest, sources = self._manifest_and_sources(observations=1)
        with open(sources[0].path, "ab") as handle:
            handle.write(_observation_line(4096))
        with self.assertRaises(ReplayValidationError):
            self._replayer(FakeChannel()).replay(sources[0])

    def test_token_estimate_mirrors_core(self) -> None:
        self.assertEqual(_estimate_tokens(""), 0)
        self.assertEqual(_estimate_tokens("ab"), 1)
        self.assertEqual(_estimate_tokens("x" * 400), 100)

    def test_failed_summarize_does_not_consume_sampling_seq(self) -> None:
        # Mirrors PromptCompressor: seq increments only after a successful
        # summarize, so transport failures must not shift later sampling.
        class SummarizeFailsOnce(FakeChannel):
            def __init__(self) -> None:
                super().__init__()
                self.failed = False

            def complete(self, prompt: str) -> ChannelReply:
                if '"intent"' not in prompt and not self.failed:
                    self.failed = True
                    raise ChannelError("injected summarize failure")
                return super().complete(prompt)

        manifest, sources = self._manifest_and_sources(observations=2)
        recorded: list[int] = []

        class RecordingSampler(SeededRateSampler):
            def should_verify(self, seq: int) -> bool:
                recorded.append(seq)
                return False

        replayer = self._replayer(SummarizeFailsOnce())
        original = run_nap_replay.SeededRateSampler
        run_nap_replay.SeededRateSampler = RecordingSampler
        try:
            result = replayer.replay(sources[0])
        finally:
            run_nap_replay.SeededRateSampler = original
        # Two observations, first summarize fails: only one seq consumed.
        self.assertEqual(recorded, [1])
        self.assertGreater(result["untouched_raw_tokens"], 0)


class ChannelArgvTests(unittest.TestCase):
    def _capture_argv(self, channel, reply: bytes = b"ok") -> list[str]:
        captured: dict[str, object] = {}

        def fake_run(argv, **kwargs):
            captured["argv"] = argv
            captured["input"] = kwargs.get("input")

            class Result:
                returncode = 0
                stdout = reply
                stderr = b""

            for arg in argv:
                if str(arg).endswith(".txt"):
                    Path(str(arg)).write_bytes(reply)
            return Result()

        original = run_nap_replay.subprocess.run
        run_nap_replay.subprocess.run = fake_run
        try:
            channel.complete("PROMPT")
        finally:
            run_nap_replay.subprocess.run = original
            channel.close()
        return captured["argv"], captured["input"]

    def test_codex_argv_shape(self) -> None:
        argv, stdin = self._capture_argv(run_nap_replay.CodexChannel("gpt-5.6-luna"))
        self.assertEqual(argv[:2], ["codex", "exec"])
        for flag in ("--sandbox", "--ephemeral", "--skip-git-repo-check"):
            self.assertIn(flag, argv)
        self.assertEqual(argv[-1], "-")
        self.assertEqual(stdin, b"PROMPT")

    def test_claude_argv_shape(self) -> None:
        argv, stdin = self._capture_argv(run_nap_replay.ClaudeChannel("haiku"))
        self.assertEqual(argv[0:2], ["claude", "-p"])
        self.assertIn("--dangerously-skip-permissions", argv)
        self.assertIn("--output-format", argv)
        self.assertNotIn("--tools", argv)
        self.assertEqual(stdin, b"PROMPT")


class StateResumeTests(unittest.TestCase):
    def test_corrupt_state_is_discarded_not_fatal(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            state_dir = Path(tmp)
            os.chmod(state_dir, 0o700)
            bad = state_dir / "session_deadbeefdeadbeefdeadbeefdeadbeef.json"
            bad.write_text("{truncated", encoding="utf-8")
            result = run_nap_replay._load_state(
                state_dir, "session_deadbeefdeadbeefdeadbeefdeadbeef"
            )
            self.assertIsNone(result)
            self.assertFalse(bad.exists())


if __name__ == "__main__":
    unittest.main()
