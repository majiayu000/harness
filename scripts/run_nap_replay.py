#!/usr/bin/env python3
"""Execute the GH1574 NAP A/B replay through an agent-runtime channel.

Reads the private sessions selected by a ``build-manifest`` run, replays
each qualifying observation through the NAP-Lite compression contract
(summarize, sampled next-action-sketch verification, failure fallback,
per-session circuit breaker), and writes the per-session evidence file
consumed by ``evaluate_nap_replay.py summarize-evidence``.

Model access goes through an agent runtime by default (``codex exec`` on a
subscription seat, read-only sandbox, ephemeral session) so the replay does
not require metered API spend. ``claude`` and an OpenAI-compatible ``api``
channel are optional alternatives.

Compression semantics mirror ``harness_core::compress::PromptCompressor``:
the same summarize/sketch prompts as ``ApiCompressModel``, 2-of-3 sketch
field agreement, deterministic seeded sampling per session, a >15% NAP
failure breaker after >=5 checks, byte/4 token estimates, and a 2048-byte
minimum observation size.

Success oracle (proxy, pending a human-approved task oracle):
``baseline_success`` is true because every manifest session completed
historically; ``candidate_success`` is false only when the session tripped
the NAP failure breaker, since every individual mismatch already falls back
to raw text and preserves behavior.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import date
from pathlib import Path
from typing import Any

from evaluate_nap_replay import (
    DEFAULT_SALT_ENV,
    OBSERVATION_FIELDS,
    SCHEMA_VERSION,
    _observation_bytes,
    _validate_manifest,
    _walk_jsonl,
)
from nap_replay_safety import (
    MAX_JSONL_LINE_BYTES,
    MAX_RECORDS_PER_SESSION,
    ReplayValidationError,
    _json_loads,
    _load_salt,
    _strict_json_load,
    secure_write_json,
)

DEFAULT_MODEL = "gpt-5.6-luna"
DEFAULT_MIN_SIZE_BYTES = 2048
DEFAULT_NAP_SAMPLE_RATE = 0.10
BREAKER_MIN_CHECKS = 5
BREAKER_FAILURE_RATE = 0.15
SKETCH_AGREEMENT_MIN = 2
SAMPLING_UNITS = 1 << 53
AGENT_TIMEOUT_SECS = 300
MAX_MODEL_REPLY_BYTES = 1024 * 1024
STATE_KEYS = {
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


def _estimate_tokens(content: str) -> int:
    """Mirror ``harness_core::compress::estimate_tokens`` (bytes / 4)."""
    if not content:
        return 0
    return max(len(content.encode("utf-8")) // 4, 1)


def _fnv1a64(seed: str) -> int:
    value = 0xCBF29CE484222325
    for byte in seed.encode("utf-8"):
        value = ((value ^ byte) * 0x00000100000001B3) & 0xFFFFFFFFFFFFFFFF
    return value


class SeededRateSampler:
    """Port of ``harness_core::compress::SeededRateSampler``."""

    def __init__(self, rate: float, seed: str) -> None:
        if rate != rate:  # NaN
            rate = 0.0
        rate = min(max(rate, 0.0), 1.0)
        self.rate_units = round(rate * SAMPLING_UNITS)
        self.phase_units = _fnv1a64(seed) >> 11

    def should_verify(self, seq: int) -> bool:
        if seq == 0 or self.rate_units == 0:
            return False
        if self.rate_units >= SAMPLING_UNITS:
            return True
        current = (seq * self.rate_units + self.phase_units) // SAMPLING_UNITS
        previous = ((seq - 1) * self.rate_units + self.phase_units) // SAMPLING_UNITS
        return current > previous


def _summarize_prompt(raw: str, task_summary: str) -> str:
    """Mirror ``harness-agents::compress_model::summarize_prompt``."""
    return (
        "You compress an agent observation without changing what the agent "
        f"should do next. Task context: {task_summary}\n\n"
        "Rewrite the observation below as a compact rendition that preserves "
        "every next-action-relevant fact: error messages, file paths, "
        "commands, identifiers, counts, and outcomes. Drop repetition, "
        "boilerplate, and progress noise. Output ONLY the compressed text, "
        f"no preamble.\n\n<observation>\n{raw}\n</observation>"
    )


def _sketch_prompt(observation: str, task_summary: str) -> str:
    """Mirror ``harness-agents::compress_model::sketch_prompt``."""
    return (
        f"Task context: {task_summary}\n\nGiven the observation below, state "
        'the single most likely next action as strict JSON with exactly these '
        'keys: {"intent": string, "target_files": [string], '
        '"command_class": string}. Output ONLY the JSON.\n\n'
        f"<observation>\n{observation}\n</observation>"
    )


def _parse_sketch(reply: str) -> dict[str, Any]:
    """Mirror ``harness-agents::compress_model::parse_sketch``."""
    start = reply.find("{")
    end = reply.rfind("}")
    if start < 0 or end < start:
        raise ReplayValidationError("no JSON object in sketch reply")
    sketch = _json_loads(reply[start : end + 1])
    if not isinstance(sketch, dict):
        raise ReplayValidationError("sketch reply is not an object")
    intent = sketch.get("intent")
    command_class = sketch.get("command_class")
    target_files = sketch.get("target_files", [])
    if not isinstance(intent, str) or not isinstance(command_class, str):
        raise ReplayValidationError("sketch intent/command_class must be strings")
    if not isinstance(target_files, list) or any(
        not isinstance(item, str) for item in target_files
    ):
        raise ReplayValidationError("sketch target_files must be a string list")
    return {
        "intent": intent,
        "target_files": target_files,
        "command_class": command_class,
    }


def _sketch_agreement(a: dict[str, Any], b: dict[str, Any]) -> int:
    """Mirror ``harness_core::compress::ActionSketch::agreement``."""
    agree = 0
    if a["intent"].lower() == b["intent"].lower():
        agree += 1
    if a["command_class"].lower() == b["command_class"].lower():
        agree += 1
    if sorted(a["target_files"]) == sorted(b["target_files"]):
        agree += 1
    return agree


class ChannelError(RuntimeError):
    """The model channel failed for one call; the caller keeps raw text."""


@dataclass
class ChannelReply:
    text: str


class CodexChannel:
    """Agent-runtime channel via ``codex exec`` (subscription seat).

    Read-only sandbox, empty working directory, and ``--ephemeral`` so the
    replay neither executes observation-embedded instructions nor writes
    new rollout files into the sessions corpus it is measuring.
    """

    def __init__(self, model: str, binary: str = "codex") -> None:
        self.model = model
        self.binary = binary
        self.workdir = Path(tempfile.mkdtemp(prefix="nap-replay-codex-"))

    def complete(self, prompt: str) -> ChannelReply:
        with tempfile.NamedTemporaryFile(
            mode="r", suffix=".txt", dir=self.workdir, delete=False
        ) as out:
            out_path = Path(out.name)
        try:
            result = subprocess.run(
                [
                    self.binary,
                    "exec",
                    "--model",
                    self.model,
                    "--sandbox",
                    "read-only",
                    "--skip-git-repo-check",
                    "--ephemeral",
                    "--cd",
                    str(self.workdir),
                    "--output-last-message",
                    str(out_path),
                    "-",
                ],
                input=prompt.encode("utf-8"),
                capture_output=True,
                timeout=AGENT_TIMEOUT_SECS,
            )
            if result.returncode != 0:
                stderr = result.stderr.decode("utf-8", "replace")[-500:]
                raise ChannelError(f"codex exec failed rc={result.returncode}: {stderr}")
            if out_path.stat().st_size > MAX_MODEL_REPLY_BYTES:
                raise ChannelError("model reply exceeds the size limit")
            return ChannelReply(out_path.read_text("utf-8").strip())
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ChannelError(f"codex exec transport failure: {exc}") from exc
        finally:
            out_path.unlink(missing_ok=True)

    def close(self) -> None:
        shutil.rmtree(self.workdir, ignore_errors=True)


class ClaudeChannel:
    """Agent-runtime channel via ``claude -p`` (subscription seat)."""

    def __init__(self, model: str, binary: str = "claude") -> None:
        self.model = model
        self.binary = binary
        self.workdir = Path(tempfile.mkdtemp(prefix="nap-replay-claude-"))

    def complete(self, prompt: str) -> ChannelReply:
        try:
            result = subprocess.run(
                [self.binary, "-p", "--model", self.model, "--tools", ""],
                input=prompt.encode("utf-8"),
                capture_output=True,
                timeout=AGENT_TIMEOUT_SECS,
                cwd=self.workdir,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ChannelError(f"claude transport failure: {exc}") from exc
        if result.returncode != 0:
            stderr = result.stderr.decode("utf-8", "replace")[-500:]
            raise ChannelError(f"claude failed rc={result.returncode}: {stderr}")
        if len(result.stdout) > MAX_MODEL_REPLY_BYTES:
            raise ChannelError("model reply exceeds the size limit")
        return ChannelReply(result.stdout.decode("utf-8", "replace").strip())

    def close(self) -> None:
        shutil.rmtree(self.workdir, ignore_errors=True)


class OpenAiCompatChannel:
    """Optional metered channel: OpenAI-compatible chat completions."""

    def __init__(self, model: str, base_url: str, api_key_env: str) -> None:
        key = os.environ.get(api_key_env, "")
        if not key:
            raise ReplayValidationError(f"api channel requires {api_key_env}")
        self.model = model
        self.url = base_url.rstrip("/") + "/chat/completions"
        self.key = key

    def complete(self, prompt: str) -> ChannelReply:
        body = json.dumps(
            {
                "model": self.model,
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0,
            }
        ).encode("utf-8")
        request = urllib.request.Request(
            self.url,
            data=body,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {self.key}",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=AGENT_TIMEOUT_SECS) as response:
                raw = response.read(MAX_MODEL_REPLY_BYTES + 1)
        except OSError as exc:
            raise ChannelError(f"api transport failure: {exc}") from exc
        if len(raw) > MAX_MODEL_REPLY_BYTES:
            raise ChannelError("model reply exceeds the size limit")
        try:
            payload = json.loads(raw.decode("utf-8"))
            text = payload["choices"][0]["message"]["content"]
        except (ValueError, KeyError, IndexError, TypeError) as exc:
            raise ChannelError(f"api reply parse failure: {exc}") from exc
        if not isinstance(text, str):
            raise ChannelError("api reply content is not text")
        return ChannelReply(text.strip())

    def close(self) -> None:  # pragma: no cover - nothing to release
        return None


def _observation_text(record: dict[str, Any]) -> str | None:
    """Concatenated content for records ``_observation_bytes`` counts."""
    if _observation_bytes(record) is None:
        return None
    observation = record
    if record.get("type") in {"response_item", "event_msg"}:
        observation = record["payload"]
    fields = OBSERVATION_FIELDS[observation.get("type")]
    parts: list[str] = []
    for field in fields:
        if field not in observation:
            continue
        value = observation[field]
        if isinstance(value, str):
            parts.append(value)
        elif value is not None:
            parts.append(
                json.dumps(
                    value,
                    sort_keys=True,
                    separators=(",", ":"),
                    ensure_ascii=False,
                    allow_nan=False,
                )
            )
    return "\n".join(parts)


@dataclass
class SessionSource:
    session_key: str
    path: Path
    source_hmac: str
    observation_records: int


class SessionReplayer:
    """Replays one manifest session through the compression contract."""

    def __init__(
        self,
        channel: Any,
        salt: bytes,
        *,
        min_size_bytes: int,
        nap_sample_rate: float,
        task_summary: str,
    ) -> None:
        self.channel = channel
        self.salt = salt
        self.min_size_bytes = min_size_bytes
        self.nap_sample_rate = nap_sample_rate
        self.task_summary = task_summary

    def replay(self, source: SessionSource) -> dict[str, Any]:
        sampler = SeededRateSampler(self.nap_sample_rate, source.session_key)
        digest_key = hmac.new(
            self.salt, b"harness.nap-replay.source-content.v1", hashlib.sha256
        ).digest()
        digest = hmac.new(digest_key, digestmod=hashlib.sha256)
        totals = {
            "baseline_tokens": 0,
            "compressed_tokens": 0,
            "fallback_raw_tokens": 0,
            "untouched_raw_tokens": 0,
            "nap_checked": 0,
            "nap_failed": 0,
            "compressor_input_tokens": 0,
            "compressor_output_tokens": 0,
        }
        breaker_tripped = False
        seq = 0
        records = 0
        with open(source.path, "rb") as handle:
            while True:
                raw_line = handle.readline(MAX_JSONL_LINE_BYTES + 1)
                if not raw_line:
                    break
                records += 1
                if records > MAX_RECORDS_PER_SESSION:
                    raise ReplayValidationError("session exceeds the record-count limit")
                digest.update(raw_line)
                record = _json_loads(raw_line.decode("utf-8"))
                if not isinstance(record, dict):
                    raise ReplayValidationError("every JSONL record must be an object")
                text = _observation_text(record)
                if text is None:
                    continue
                raw_tokens = _estimate_tokens(text)
                totals["baseline_tokens"] += raw_tokens
                if len(text.encode("utf-8")) < self.min_size_bytes or breaker_tripped:
                    totals["untouched_raw_tokens"] += raw_tokens
                    continue
                seq += 1
                self._replay_observation(text, raw_tokens, seq, sampler, totals)
                breaker_tripped = self._breaker_open(totals)
        if not hmac.compare_digest(digest.hexdigest(), source.source_hmac):
            raise ReplayValidationError(
                "session content no longer matches the manifest source hmac"
            )
        if totals["baseline_tokens"] == 0:
            raise ReplayValidationError("session produced no baseline tokens")
        return {
            "session_key": source.session_key,
            **totals,
            "baseline_success": True,
            "candidate_success": not breaker_tripped,
        }

    def _breaker_open(self, totals: dict[str, int]) -> bool:
        checked = totals["nap_checked"]
        if checked < BREAKER_MIN_CHECKS:
            return False
        return totals["nap_failed"] / checked > BREAKER_FAILURE_RATE

    def _complete(self, prompt: str, totals: dict[str, int]) -> str:
        reply = self.channel.complete(prompt)
        totals["compressor_input_tokens"] += _estimate_tokens(prompt)
        totals["compressor_output_tokens"] += _estimate_tokens(reply.text)
        return reply.text

    def _replay_observation(
        self,
        text: str,
        raw_tokens: int,
        seq: int,
        sampler: SeededRateSampler,
        totals: dict[str, int],
    ) -> None:
        try:
            compressed = self._complete(
                _summarize_prompt(text, self.task_summary), totals
            )
        except ChannelError:
            totals["untouched_raw_tokens"] += raw_tokens
            return
        compressed_tokens = _estimate_tokens(compressed)
        if not sampler.should_verify(seq):
            totals["compressed_tokens"] += compressed_tokens
            return
        try:
            raw_sketch = _parse_sketch(
                self._complete(_sketch_prompt(text, self.task_summary), totals)
            )
            compressed_sketch = _parse_sketch(
                self._complete(_sketch_prompt(compressed, self.task_summary), totals)
            )
        except (ChannelError, ReplayValidationError):
            totals["untouched_raw_tokens"] += raw_tokens
            return
        totals["nap_checked"] += 1
        if _sketch_agreement(raw_sketch, compressed_sketch) >= SKETCH_AGREEMENT_MIN:
            totals["compressed_tokens"] += compressed_tokens
        else:
            totals["nap_failed"] += 1
            totals["fallback_raw_tokens"] += raw_tokens


def _manifest_window(manifest: dict[str, Any]) -> tuple[date | None, date | None]:
    selection = manifest["selection"]
    since = selection["since_date"]
    through = selection["through_date"]
    return (
        date.fromisoformat(since) if since else None,
        date.fromisoformat(through) if through else None,
    )


def _locate_sessions(
    manifest: dict[str, Any], sessions_root: Path, salt: bytes
) -> list[SessionSource]:
    since, through = _manifest_window(manifest)
    candidates = _walk_jsonl(sessions_root, salt, since, through)
    by_key = {candidate.session_key: candidate for candidate in candidates}
    sources: list[SessionSource] = []
    for session in manifest["sessions"]:
        candidate = by_key.get(session["session_key"])
        if candidate is None:
            raise ReplayValidationError(
                "a manifest session is missing from the sessions root"
            )
        sources.append(
            SessionSource(
                session_key=session["session_key"],
                path=candidate.path,
                source_hmac=session["source_hmac_sha256"],
                observation_records=session["observation_records"],
            )
        )
    return sources


def _load_state(state_dir: Path, session_key: str) -> dict[str, Any] | None:
    path = state_dir / f"{session_key}.json"
    if not path.exists():
        return None
    state = _strict_json_load(path)
    if not isinstance(state, dict) or set(state) != STATE_KEYS:
        raise ReplayValidationError(f"corrupt replay state for {session_key}")
    return state


def _build_channel(args: argparse.Namespace) -> Any:
    if args.channel == "codex":
        return CodexChannel(args.model)
    if args.channel == "claude":
        return ClaudeChannel(args.model)
    return OpenAiCompatChannel(args.model, args.api_base_url, args.api_key_env)


def run(args: argparse.Namespace) -> int:
    salt = _load_salt(args.salt_env)
    manifest = _validate_manifest(_strict_json_load(args.manifest))
    state_dir: Path = args.state_dir
    state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    if os.stat(state_dir).st_mode & 0o077:
        raise ReplayValidationError("state dir must be owner-only mode 0700")
    sources = _locate_sessions(manifest, args.sessions_root, salt)
    pending = [
        source for source in sources if _load_state(state_dir, source.session_key) is None
    ]
    if args.smallest_first:
        pending.sort(key=lambda source: source.observation_records)
    if args.session_limit is not None:
        pending = pending[: args.session_limit]
    channel = _build_channel(args)
    replayer = SessionReplayer(
        channel,
        salt,
        min_size_bytes=args.min_size_bytes,
        nap_sample_rate=args.nap_sample_rate,
        task_summary=args.task_summary,
    )

    def replay_one(source: SessionSource) -> str:
        result = replayer.replay(source)
        secure_write_json(state_dir / f"{source.session_key}.json", result)
        return source.session_key

    try:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            for index, key in enumerate(pool.map(replay_one, pending), start=1):
                print(f"[{index}/{len(pending)}] replayed {key}", file=sys.stderr)
    finally:
        channel.close()

    results = []
    for source in sources:
        state = _load_state(state_dir, source.session_key)
        if state is None:
            print(
                f"progress: {source.session_key} still pending; rerun to resume",
                file=sys.stderr,
            )
        else:
            results.append(state)
    if len(results) != len(sources):
        print(
            f"evidence not written: {len(results)}/{len(sources)} sessions complete",
            file=sys.stderr,
        )
        return 4
    evidence = {
        "schema_version": SCHEMA_VERSION,
        "manifest_id": manifest["manifest_id"],
        "expected_sessions": manifest["selection"]["expected_sessions"],
        "pricing": {
            "compressor_input_usd_per_million": args.pricing_compressor_input,
            "compressor_output_usd_per_million": args.pricing_compressor_output,
            "saved_input_usd_per_million": args.pricing_saved_input,
        },
        "sessions": results,
    }
    secure_write_json(args.output, evidence)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--sessions-root", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--salt-env", default=DEFAULT_SALT_ENV)
    parser.add_argument("--channel", choices=("codex", "claude", "api"), default="codex")
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--min-size-bytes", type=int, default=DEFAULT_MIN_SIZE_BYTES)
    parser.add_argument("--nap-sample-rate", type=float, default=DEFAULT_NAP_SAMPLE_RATE)
    parser.add_argument("--session-limit", type=int, default=None)
    parser.add_argument(
        "--smallest-first",
        action="store_true",
        help="replay smaller sessions first (useful for pilot runs)",
    )
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--task-summary", default="replayed Codex session")
    parser.add_argument("--api-base-url", default="https://api.openai.com/v1")
    parser.add_argument("--api-key-env", default="OPENAI_API_KEY")
    parser.add_argument(
        "--pricing-compressor-input",
        type=float,
        required=True,
        help="USD per million compressor input tokens (0 for subscription seats)",
    )
    parser.add_argument(
        "--pricing-compressor-output",
        type=float,
        required=True,
        help="USD per million compressor output tokens (0 for subscription seats)",
    )
    parser.add_argument(
        "--pricing-saved-input",
        type=float,
        required=True,
        help="USD per million saved downstream input tokens (must be positive)",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return run(args)
    except ReplayValidationError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except (OSError, UnicodeError, RecursionError, OverflowError, MemoryError, ValueError):
        print("error: unsafe or invalid replay input", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
