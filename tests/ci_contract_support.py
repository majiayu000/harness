"""Restricted YAML parsing and an exact execution schema for the CI workflow."""

from __future__ import annotations

import re
from dataclasses import dataclass
from textwrap import dedent
from typing import TypeAlias


@dataclass(frozen=True)
class BlockScalar:
    style: str
    body: str


YamlValue: TypeAlias = str | BlockScalar | dict[str, "YamlValue"] | list["YamlValue"]

_KEY_VALUE = re.compile(r"^([A-Za-z0-9_-]+):(?:[ \t]+(.*))?$")
_BLOCK_STYLES = {"|", "|-", ">", ">-"}


class RestrictedYamlParser:
    """Parse the mapping/list/block-scalar subset used by ci.yml.

    Rejecting unsupported YAML is intentional: aliases, merge keys, duplicate
    keys, flow mappings, and alternate execution spellings must not bypass the
    exact CI contract.
    """

    def __init__(self, source: str) -> None:
        self.lines = source.splitlines()

    def parse(self) -> dict[str, YamlValue]:
        value, index = self._parse_mapping(0, 0)
        index = self._skip_trivia(index)
        if index != len(self.lines):
            self._fail(index, "unexpected content after top-level mapping")
        return value

    def _parse_mapping(
        self, index: int, indent: int
    ) -> tuple[dict[str, YamlValue], int]:
        result: dict[str, YamlValue] = {}
        while True:
            index = self._skip_trivia(index)
            if index >= len(self.lines):
                break
            line_indent, text = self._line(index)
            if line_indent < indent:
                break
            if line_indent > indent:
                self._fail(index, f"expected mapping indentation {indent}")
            if text.startswith("- "):
                break

            key, raw_value = self._parse_key_value(index, text)
            if key in result:
                self._fail(index, f"duplicate mapping key {key!r}")
            result[key], index = self._parse_value(index + 1, indent, raw_value)
        return result, index

    def _parse_sequence(
        self, index: int, indent: int
    ) -> tuple[list[YamlValue], int]:
        result: list[YamlValue] = []
        while True:
            index = self._skip_trivia(index)
            if index >= len(self.lines):
                break
            line_indent, text = self._line(index)
            if line_indent < indent:
                break
            if line_indent != indent or not text.startswith("- "):
                self._fail(index, f"expected sequence indentation {indent}")

            item_text = text[2:]
            match = _KEY_VALUE.fullmatch(item_text)
            if match is None:
                result.append(item_text)
                index += 1
                continue

            item: dict[str, YamlValue] = {}
            key, raw_value = match.groups()
            item[key], index = self._parse_value(index + 1, indent + 2, raw_value)

            while True:
                next_index = self._skip_trivia(index)
                if next_index >= len(self.lines):
                    index = next_index
                    break
                next_indent, next_text = self._line(next_index)
                if next_indent <= indent:
                    index = next_index
                    break
                if next_indent != indent + 2:
                    self._fail(next_index, f"expected sequence item indentation {indent + 2}")
                next_key, next_raw_value = self._parse_key_value(
                    next_index, next_text
                )
                if next_key in item:
                    self._fail(next_index, f"duplicate sequence item key {next_key!r}")
                item[next_key], index = self._parse_value(
                    next_index + 1, indent + 2, next_raw_value
                )
            result.append(item)
        return result, index

    def _parse_value(
        self, index: int, key_indent: int, raw_value: str | None
    ) -> tuple[YamlValue, int]:
        if raw_value in _BLOCK_STYLES:
            return self._parse_block_scalar(index, key_indent, raw_value)
        if raw_value is not None:
            return raw_value, index

        child_index = self._skip_trivia(index)
        if child_index >= len(self.lines):
            self._fail(index - 1, "mapping key has no value")
        child_indent, child_text = self._line(child_index)
        expected_indent = key_indent + 2
        if child_indent != expected_indent:
            self._fail(child_index, f"expected child indentation {expected_indent}")
        if child_text.startswith("- "):
            return self._parse_sequence(child_index, expected_indent)
        return self._parse_mapping(child_index, expected_indent)

    def _parse_block_scalar(
        self, index: int, key_indent: int, style: str
    ) -> tuple[BlockScalar, int]:
        end = index
        while end < len(self.lines):
            raw = self.lines[end]
            if raw.strip() and self._indent(raw, end) <= key_indent:
                break
            end += 1

        content = self.lines[index:end]
        while content and not content[-1].strip():
            content.pop()
        nonblank_indents = [
            self._indent(line, index + offset)
            for offset, line in enumerate(content)
            if line.strip()
        ]
        if not nonblank_indents:
            body = ""
        else:
            content_indent = min(nonblank_indents)
            if content_indent <= key_indent:
                self._fail(index, "block scalar content must be indented")
            body = "\n".join(
                line[content_indent:] if line.strip() else "" for line in content
            )
        return BlockScalar(style, body), end

    def _parse_key_value(self, index: int, text: str) -> tuple[str, str | None]:
        match = _KEY_VALUE.fullmatch(text)
        if match is None:
            self._fail(index, "expected an unquoted mapping key")
        return match.groups()

    def _skip_trivia(self, index: int) -> int:
        while index < len(self.lines):
            stripped = self.lines[index].strip()
            if stripped and not stripped.startswith("#"):
                break
            index += 1
        return index

    def _line(self, index: int) -> tuple[int, str]:
        raw = self.lines[index]
        indent = self._indent(raw, index)
        return indent, raw[indent:]

    def _indent(self, raw: str, index: int) -> int:
        prefix = raw[: len(raw) - len(raw.lstrip())]
        if "\t" in prefix:
            self._fail(index, "tabs are not allowed for YAML indentation")
        return len(prefix)

    def _fail(self, index: int, message: str) -> None:
        raise AssertionError(f"ci.yml line {index + 1}: {message}")


def block(body: str, style: str = "|") -> BlockScalar:
    return BlockScalar(style, dedent(body).strip("\n"))


RUST_OR_CI_CHANGED = (
    "needs.changed.outputs.rust == 'true' || needs.changed.outputs.ci == 'true'"
)
DATABASE_URL = "postgres://postgres:postgres@localhost:5432/harness_test"

FILTERS = block(
    """\
    rust:
      - 'crates/**'
      - 'Cargo.toml'
      - 'Cargo.lock'
      - 'scripts/check_storage_legacy_openers.py'
      - 'scripts/test-server-*.sh'
    server:
      - 'crates/harness-server/**'
      - 'crates/harness-core/**'
      - 'crates/harness-workflow/**'
      - 'scripts/test-server-*.sh'
    agents:
      - 'crates/harness-agents/**'
      - 'crates/harness-core/**'
    ci:
      - '.github/workflows/**'
      - '.bun-version'
      - 'scripts/check_ci_results.py'
      - 'scripts/check_committed_whitespace.py'
      - 'scripts/check_storage_legacy_openers.py'
    workspace:
      - 'Cargo.toml'
      - 'Cargo.lock'
      - '.github/workflows/**'
    other_crates:
      - 'crates/**'
      - '!crates/harness-server/**'
      - '!crates/harness-core/**'
      - '!crates/harness-workflow/**'
      - '!crates/harness-agents/**'
    """
)

TEST_SCOPE = block(
    """\
    # Full workspace when workspace-level files or unclassified crates
    # changed; otherwise scope cargo test to the affected package sets.
    # harness-server is always excluded from `cargo test` because its
    # tests run via the dedicated fast/db profile scripts below.
    if [ "$WORKSPACE_CHANGED" = "true" ] || [ "$CI_CHANGED" = "true" ] || [ "$OTHER_CRATES_CHANGED" = "true" ]; then
      echo "packages=--workspace --exclude harness-server" >> "$GITHUB_OUTPUT"
      echo "run_server=true" >> "$GITHUB_OUTPUT"
      exit 0
    fi
    pkgs=""
    run_server=false
    if [ "$SERVER_CHANGED" = "true" ]; then
      pkgs="-p harness-core -p harness-workflow"
      run_server=true
    fi
    if [ "$AGENTS_CHANGED" = "true" ]; then
      # harness-server depends on harness-agents, so agent changes must
      # also run the server test profiles: a behavioral regression in
      # the agent adapters would otherwise ship untested (clippy only
      # catches compile breakage).
      pkgs="$pkgs -p harness-agents"
      run_server=true
      case "$pkgs" in
        *"-p harness-core"*) ;;
        *) pkgs="$pkgs -p harness-core" ;;
      esac
    fi
    if [ -z "$pkgs" ]; then
      # Rust-adjacent change (e.g. helper scripts) with no crate match:
      # fall back to the full workspace run to stay safe.
      pkgs="--workspace --exclude harness-server"
      run_server=true
    fi
    echo "packages=$pkgs" >> "$GITHUB_OUTPUT"
    echo "run_server=$run_server" >> "$GITHUB_OUTPUT"
    """
)

CI_RESULT_ENV = {
    "HARNESS_CI_RESULT_CHANGED": "${{ needs.changed.result }}",
    "HARNESS_CI_RESULT_STORAGE_LEGACY_OPENERS": (
        "${{ needs.storage-legacy-openers.result }}"
    ),
    "HARNESS_CI_RESULT_REPOSITORY_CHECKS": "${{ needs.repository-checks.result }}",
    "HARNESS_CI_RESULT_FMT": "${{ needs.fmt.result }}",
    "HARNESS_CI_RESULT_WEB_BUILD": "${{ needs.web-build.result }}",
    "HARNESS_CI_RESULT_CLIPPY": "${{ needs.clippy.result }}",
    "HARNESS_CI_RESULT_TEST": "${{ needs.test.result }}",
    "HARNESS_CI_RESULT_AUDIT": "${{ needs.audit.result }}",
}

EXPECTED_JOBS: dict[str, YamlValue] = {
    "changed": {
        "name": "Detect Changes",
        "runs-on": "ubuntu-latest",
        "outputs": {
            name: f"${{{{ steps.filter.outputs.{name} }}}}"
            for name in ("rust", "server", "agents", "ci", "workspace", "other_crates")
        },
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "uses": "dorny/paths-filter@v3",
                "id": "filter",
                "with": {"filters": FILTERS},
            },
        ],
    },
    "storage-legacy-openers": {
        "name": "Storage Legacy Openers",
        "runs-on": "ubuntu-latest",
        "needs": "changed",
        "if": RUST_OR_CI_CHANGED,
        "steps": [
            {"uses": "actions/checkout@v4"},
            {"run": "python3 scripts/check_storage_legacy_openers.py --self-test"},
            {"run": "python3 scripts/check_storage_legacy_openers.py"},
        ],
    },
    "repository-checks": {
        "name": "Repository Checks",
        "runs-on": "ubuntu-latest",
        "steps": [
            {"uses": "actions/checkout@v4", "with": {"fetch-depth": "0"}},
            {"uses": "actions/setup-python@v5", "with": {"python-version": '"3.x"'}},
            {
                "name": "Install test dependencies",
                "run": "python3 -m pip install pytest",
            },
            {
                "name": "Test repository contracts",
                "run": "python3 -m pytest -q tests",
            },
            {
                "name": "Check committed whitespace",
                "run": "python3 scripts/check_committed_whitespace.py",
            },
        ],
    },
    "fmt": {
        "name": "Format",
        "runs-on": "ubuntu-latest",
        "needs": "changed",
        "if": RUST_OR_CI_CHANGED,
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "uses": "dtolnay/rust-toolchain@stable",
                "with": {"components": "rustfmt"},
            },
            {"run": "cargo fmt --all -- --check"},
        ],
    },
    "web-build": {
        "name": "Web Build",
        "runs-on": "ubuntu-latest",
        "needs": "changed",
        "if": RUST_OR_CI_CHANGED,
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "uses": "oven-sh/setup-bun@v2",
                "with": {"bun-version-file": ".bun-version"},
            },
            {
                "name": "Build web bundle",
                "working-directory": "web",
                "run": block(
                    """\
                    bun install --frozen-lockfile
                    bun run build
                    """
                ),
            },
            {
                "uses": "actions/upload-artifact@v4",
                "with": {
                    "name": "web-dist",
                    "path": "web/dist",
                    "if-no-files-found": "error",
                    "retention-days": "1",
                },
            },
        ],
    },
    "clippy": {
        "name": "Clippy",
        "runs-on": "ubuntu-latest",
        "needs": "[changed, web-build]",
        "if": RUST_OR_CI_CHANGED,
        "env": {"HARNESS_SKIP_WEB_BUILD": '"1"'},
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "uses": "dtolnay/rust-toolchain@stable",
                "with": {"components": "clippy"},
            },
            {"uses": "Swatinem/rust-cache@v2"},
            {
                "uses": "actions/download-artifact@v4",
                "with": {"name": "web-dist", "path": "web/dist"},
            },
            {"run": "cargo clippy --workspace --all-targets -- -D warnings"},
        ],
    },
    "test": {
        "name": "Test",
        "runs-on": "ubuntu-latest",
        "needs": "[changed, web-build]",
        "if": RUST_OR_CI_CHANGED,
        "timeout-minutes": "15",
        "env": {"HARNESS_SKIP_WEB_BUILD": '"1"'},
        "services": {
            "postgres": {
                "image": "postgres:16",
                "env": {
                    "POSTGRES_USER": "postgres",
                    "POSTGRES_PASSWORD": "postgres",
                    "POSTGRES_DB": "harness_test",
                },
                "ports": ["5432:5432"],
                "options": block(
                    """\
                    --health-cmd pg_isready
                    --health-interval 10s
                    --health-timeout 5s
                    --health-retries 5
                    """,
                    style=">-",
                ),
            }
        },
        "steps": [
            {"uses": "actions/checkout@v4"},
            {"uses": "dtolnay/rust-toolchain@stable"},
            {"uses": "Swatinem/rust-cache@v2"},
            {
                "uses": "actions/download-artifact@v4",
                "with": {"name": "web-dist", "path": "web/dist"},
            },
            {
                "name": "Compute test scope",
                "id": "scope",
                "env": {
                    "WORKSPACE_CHANGED": "${{ needs.changed.outputs.workspace }}",
                    "CI_CHANGED": "${{ needs.changed.outputs.ci }}",
                    "OTHER_CRATES_CHANGED": (
                        "${{ needs.changed.outputs.other_crates }}"
                    ),
                    "SERVER_CHANGED": "${{ needs.changed.outputs.server }}",
                    "AGENTS_CHANGED": "${{ needs.changed.outputs.agents }}",
                },
                "run": TEST_SCOPE,
            },
            {
                "run": "cargo test ${{ steps.scope.outputs.packages }}",
                "env": {"HARNESS_DATABASE_URL": DATABASE_URL},
            },
            {
                "name": "Harness-server fast profile",
                "if": "steps.scope.outputs.run_server == 'true'",
                "run": "scripts/test-server-fast.sh",
                "env": {"HARNESS_DATABASE_URL": DATABASE_URL},
            },
            {
                "name": "Harness-server full DB profile",
                "if": "steps.scope.outputs.run_server == 'true'",
                "run": "scripts/test-server-db.sh",
                "env": {"HARNESS_DATABASE_URL": DATABASE_URL},
            },
        ],
    },
    "audit": {
        "name": "Security Audit",
        "runs-on": "ubuntu-latest",
        "needs": "changed",
        "if": RUST_OR_CI_CHANGED,
        "permissions": {"contents": "read", "checks": "write"},
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "uses": "rustsec/audit-check@v2.0.0",
                "with": {"token": "${{ secrets.GITHUB_TOKEN }}"},
            },
        ],
    },
    "ci-result": {
        "name": "CI Result",
        "runs-on": "ubuntu-latest",
        "if": "always()",
        "needs": (
            "[changed, storage-legacy-openers, repository-checks, fmt, "
            "web-build, clippy, test, audit]"
        ),
        "steps": [
            {"uses": "actions/checkout@v4"},
            {
                "name": "Check all jobs",
                "env": CI_RESULT_ENV,
                "run": "python3 scripts/check_ci_results.py",
            },
        ],
    },
}

EXPECTED_WORKFLOW: dict[str, YamlValue] = {
    "name": "CI",
    "on": {
        "push": {"branches": "[main]"},
        "pull_request": {"branches": "[main]"},
    },
    "env": {"CARGO_TERM_COLOR": "always"},
    "jobs": EXPECTED_JOBS,
}


def parse_workflow(source: str) -> dict[str, YamlValue]:
    return RestrictedYamlParser(source).parse()


def assert_tree_equal(actual: YamlValue, expected: YamlValue, path: str = "ci") -> None:
    assert type(actual) is type(expected), (
        f"{path} type changed: expected {type(expected).__name__}, "
        f"got {type(actual).__name__}"
    )
    if isinstance(expected, dict):
        assert isinstance(actual, dict)
        assert set(actual) == set(expected), (
            f"{path} keys changed: missing={sorted(expected.keys() - actual.keys())}, "
            f"extra={sorted(actual.keys() - expected.keys())}"
        )
        for key, expected_value in expected.items():
            assert_tree_equal(actual[key], expected_value, f"{path}.{key}")
    elif isinstance(expected, list):
        assert isinstance(actual, list)
        assert len(actual) == len(expected), (
            f"{path} item count changed: expected {len(expected)}, got {len(actual)}"
        )
        for index, (actual_value, expected_value) in enumerate(
            zip(actual, expected, strict=True)
        ):
            assert_tree_equal(actual_value, expected_value, f"{path}[{index}]")
    else:
        assert actual == expected, f"{path} changed: expected {expected!r}, got {actual!r}"


def assert_ci_contract(workflow: str, hook: str) -> None:
    assert_tree_equal(parse_workflow(workflow), EXPECTED_WORKFLOW)

    hook_lines = {
        line.strip()
        for line in hook.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    for line in (
        "derive_scope() {",
        "staged=$(git diff --cached --name-only)",
        'echo "--workspace"',
        "scope=$(derive_scope)",
        "cargo clippy $scope --all-targets -- -D warnings",
    ):
        assert line in hook_lines, f"pre-commit hook is missing active line: {line}"
