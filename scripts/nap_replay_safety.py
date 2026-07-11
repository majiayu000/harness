"""Bounded private-file I/O for the local GH1574 replay evaluator."""

from __future__ import annotations

import json
import os
import re
import stat
from pathlib import Path
from typing import Any, Iterable


MAX_JSON_INPUT_BYTES = 64 * 1024 * 1024
MAX_JSONL_LINE_BYTES = 256 * 1024 * 1024
MAX_RECORDS_PER_SESSION = 5_000_000
MAX_METRIC_VALUE = 10**15

WINDOWS_ABSOLUTE_RE = re.compile(r"^[A-Za-z]:[\\/]")
WINDOWS_UNC_RE = re.compile(r"^\\\\[^\\]+\\[^\\]+")
EMBEDDED_POSIX_PATH_RE = re.compile(r"(?:^|[\s\"'=])/(?:Users|home|private|tmp|var|etc)/")
SECRET_RE = re.compile(
    r"(?:ghp_|github_pat_|sk-[A-Za-z0-9]|AKIA[0-9A-Z]{12}|"
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----|"
    r"(?:api[_-]?key|token|authorization)\s*[:=])",
    re.IGNORECASE,
)


class ReplayValidationError(ValueError):
    """Input cannot safely or completely support the requested report."""


def _reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ReplayValidationError("JSON input contains a duplicate object key")
        result[key] = value
    return result


def _bounded_json_int(raw: str) -> int:
    value = int(raw)
    if abs(value) > MAX_METRIC_VALUE:
        raise ReplayValidationError("JSON integer exceeds the supported range")
    return value


def _json_loads(text: str) -> Any:
    return json.loads(
        text,
        object_pairs_hook=_reject_duplicate_keys,
        parse_int=_bounded_json_int,
        parse_constant=lambda value: (_ for _ in ()).throw(
            ReplayValidationError(f"non-finite JSON number: {value}")
        ),
    )


def _open_directory(path: Path, *, private: bool) -> int:
    try:
        resolved = path.resolve(strict=True)
    except RuntimeError as exc:
        raise ReplayValidationError("directory path cannot be resolved safely") from exc
    expected = resolved.stat()
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(resolved, flags)
    observed = os.fstat(descriptor)
    identity = (expected.st_dev, expected.st_ino, expected.st_uid, stat.S_IMODE(expected.st_mode))
    opened = (observed.st_dev, observed.st_ino, observed.st_uid, stat.S_IMODE(observed.st_mode))
    if identity != opened or not stat.S_ISDIR(observed.st_mode):
        os.close(descriptor)
        raise ReplayValidationError("directory identity changed during secure open")
    if private and (observed.st_uid != os.geteuid() or stat.S_IMODE(observed.st_mode) & 0o077):
        os.close(descriptor)
        raise ReplayValidationError("output parent must be owner-only mode 0700")
    return descriptor


def _strict_json_load(path: Path) -> Any:
    directory_fd: int | None = None
    descriptor: int | None = None
    try:
        if path.parent.is_symlink() or path.name in {"", ".", ".."}:
            raise ReplayValidationError("JSON input path is unsafe")
        directory_fd = _open_directory(path.parent, private=False)
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path.name, flags, dir_fd=directory_fd)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size > MAX_JSON_INPUT_BYTES:
            raise ReplayValidationError("JSON input is not a bounded regular file")
        chunks: list[bytes] = []
        remaining = MAX_JSON_INPUT_BYTES + 1
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        if remaining == 0 and os.read(descriptor, 1):
            raise ReplayValidationError("JSON input exceeds the size limit")
        after = os.fstat(descriptor)
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        ):
            raise ReplayValidationError("JSON input changed while it was read")
        return _json_loads(b"".join(chunks).decode("utf-8"))
    except ReplayValidationError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError, RecursionError, ValueError) as exc:
        raise ReplayValidationError("failed to read valid bounded UTF-8 JSON input") from exc
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if directory_fd is not None:
            os.close(directory_fd)


def _assert_no_leaks(value: Any, forbidden_tokens: Iterable[str] = ()) -> None:
    forbidden = tuple(token for token in forbidden_tokens if token)

    def unsafe(text: str) -> bool:
        normalized = text.strip()
        return (
            normalized.startswith("/")
            or normalized.casefold().startswith("file://")
            or WINDOWS_ABSOLUTE_RE.match(normalized) is not None
            or WINDOWS_UNC_RE.match(normalized) is not None
            or EMBEDDED_POSIX_PATH_RE.search(text) is not None
            or SECRET_RE.search(text) is not None
            or any(token in text for token in forbidden)
            or any(ord(char) < 32 for char in text)
        )

    def visit(node: Any) -> None:
        if isinstance(node, dict):
            for key, child in node.items():
                if not isinstance(key, str) or unsafe(key):
                    raise ReplayValidationError("output contains unsafe key material")
                visit(child)
        elif isinstance(node, list):
            for child in node:
                visit(child)
        elif isinstance(node, str) and unsafe(node):
            raise ReplayValidationError("output contains private or secret-like material")

    visit(value)


def _validate_salt(salt: bytes) -> None:
    if len(salt) != 32 or len(set(salt)) < 8:
        raise ReplayValidationError("operator salt must contain 32 CSPRNG-generated bytes")


def _load_salt(env_name: str) -> bytes:
    try:
        salt = bytes.fromhex(os.environ.get(env_name, ""))
    except ValueError as exc:
        raise ReplayValidationError(
            f"operator salt in {env_name} must be 64 hexadecimal characters"
        ) from exc
    _validate_salt(salt)
    return salt


def secure_write_json(path: Path, value: Any) -> None:
    """Create one mode-0600 JSON file inside a stable private directory FD."""
    encoded = (json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n").encode()
    descriptor: int | None = None
    directory_fd: int | None = None
    created = False
    try:
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.parent.is_symlink() or path.name in {"", ".", ".."}:
            raise ReplayValidationError("output path is unsafe")
        directory_fd = _open_directory(path.parent, private=True)
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(path.name, flags, 0o600, dir_fd=directory_fd)
        created = True
        os.fchmod(descriptor, 0o600)
        view = memoryview(encoded)
        while view:
            written = os.write(descriptor, view)
            if written <= 0:
                raise ReplayValidationError("secure output write made no progress")
            view = view[written:]
        os.fsync(descriptor)
        if stat.S_IMODE(os.fstat(descriptor).st_mode) != 0o600:
            raise ReplayValidationError("output file mode is not 0600")
    except ReplayValidationError:
        if created and directory_fd is not None:
            os.unlink(path.name, dir_fd=directory_fd)
        raise
    except OSError as exc:
        if created and directory_fd is not None:
            try:
                os.unlink(path.name, dir_fd=directory_fd)
            except OSError as cleanup_exc:
                raise ReplayValidationError("failed to clean partial output") from cleanup_exc
        raise ReplayValidationError("failed to write secure output") from exc
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if directory_fd is not None:
            os.close(directory_fd)
