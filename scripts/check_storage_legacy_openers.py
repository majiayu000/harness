#!/usr/bin/env python3
"""Guard against new production uses of legacy path-derived storage openers."""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from textwrap import dedent


LEGACY_PATTERNS = (
    "legacy_schema_for_path(",
    "open_legacy_path_schema_pool(",
    "open_legacy_path_schema(",
    "StoreLocation::PathDerivedSchema",
)

PG_CONTEXT_TYPES = {"PgStoreContext"}

LEGACY_FUNCTION_NAMES = (
    "legacy_schema_for_path",
    "open_legacy_path_schema_pool",
    "open_legacy_path_schema",
)

LEGACY_STORE_TYPES = {
    "Db",
    "TaskDb",
    "TaskStore",
    "ThreadDb",
    "ProjectRegistry",
    "RuntimeStateStore",
    "EvalStore",
    "PlanDb",
    "QValueStore",
    "ReviewStore",
    "WorkflowRuntimeStore",
    "IssueWorkflowStore",
    "ProjectWorkflowStore",
    "WorkspaceLeaseStore",
}

LEGACY_STORE_OPEN_RE = re.compile(
    r"\b(?P<receiver>[A-Za-z_][A-Za-z0-9_:]*(?:::<[^>]+>)?)::"
    r"(?P<method>open|open_with_database_url)\s*\("
)
PG_CONTEXT_LEGACY_RE = re.compile(
    r"\b(?P<receiver>[A-Za-z_][A-Za-z0-9_:]*(?:::<[^>]+>)?)::"
    r"from_legacy_path_schema\s*\("
)

GLOBAL_ALLOWED_LEGACY_FUNCTIONS = re.compile(
    r"^(from_legacy_path_schema|open_legacy_path_schema_pool|open_legacy_path_schema|"
    r"legacy_schema_for_path|open_migrated|migrate_[A-Za-z0-9_]+_if_needed)$"
)

FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\b")
IMPL_FOR_RE = re.compile(
    r"\bimpl(?:\s*<[^>{}]*>)?\s+[^{};]+?\s+for\s+"
    r"(?P<target>[A-Za-z_][A-Za-z0-9_:]*(?:<[^>{}]*>)?)"
)
IMPL_INHERENT_RE = re.compile(
    r"\bimpl(?:\s*<[^>{}]*>)?\s+"
    r"(?P<target>[A-Za-z_][A-Za-z0-9_:]*(?:<[^>{}]*>)?)"
)
CFG_START_RE = re.compile(r"#\s*\[\s*cfg\s*\(")
USE_ALIAS_RE = re.compile(
    r"\b(?P<store>"
    + "|".join(sorted(LEGACY_STORE_TYPES))
    + r")\s+as\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)"
)
TYPE_ALIAS_RE = re.compile(
    r"\btype\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*[^;]*\b(?P<store>"
    + "|".join(sorted(LEGACY_STORE_TYPES))
    + r")\b[^;]*;",
    re.DOTALL,
)
PG_CONTEXT_USE_ALIAS_RE = re.compile(
    r"\bPgStoreContext\s+as\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)"
)
PG_CONTEXT_TYPE_ALIAS_RE = re.compile(
    r"\btype\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*[^;]*"
    r"\bPgStoreContext\b[^;]*;",
    re.DOTALL,
)
USE_STATEMENT_RE = re.compile(r"\buse\b[^;]*;", re.DOTALL)
LEGACY_FUNCTION_ALIAS_RE = re.compile(
    r"\b(?P<function>"
    + "|".join(sorted(LEGACY_FUNCTION_NAMES, key=len, reverse=True))
    + r")\s+as\s+(?P<alias>[A-Za-z_][A-Za-z0-9_]*)"
)


@dataclass(frozen=True)
class Violation:
    path: Path
    line_number: int
    pattern: str
    line: str

    def format(self, root: Path) -> str:
        rel_path = self.path.relative_to(root)
        return f"{rel_path}:{self.line_number}: {self.pattern}: {self.line.strip()}"


def is_test_source(path: Path) -> bool:
    parts = set(path.parts)
    name = path.name
    return (
        "tests" in parts
        or any(part.endswith("_tests") for part in path.parts)
        or name == "tests.rs"
        or name.endswith("_tests.rs")
        or name in {"test_fixtures.rs", "test_helpers.rs"}
    )


def is_comment_or_blank(line: str) -> bool:
    stripped = line.strip()
    return not stripped or stripped.startswith("//") or stripped.startswith("*")


def base_type_name(type_name: str) -> str:
    return type_name.split("::")[-1].split("<", 1)[0].strip()


def receiver_type_name(receiver: str) -> str:
    return re.sub(r"::<[^>]+>$", "", receiver).split("::")[-1]


def impl_type_name(code: str) -> str | None:
    match = IMPL_FOR_RE.search(code)
    if match:
        return base_type_name(match.group("target"))
    match = IMPL_INHERENT_RE.search(code)
    if match:
        return base_type_name(match.group("target"))
    return None


def legacy_store_receivers(lines: list[str]) -> set[str]:
    receivers = set(LEGACY_STORE_TYPES)
    code = "\n".join(line.split("//", 1)[0] for line in lines)
    for use_statement in USE_STATEMENT_RE.finditer(code):
        for alias_match in USE_ALIAS_RE.finditer(use_statement.group(0)):
            receivers.add(alias_match.group("alias"))
    for alias_match in TYPE_ALIAS_RE.finditer(code):
        receivers.add(alias_match.group("alias"))
    return receivers


def pg_context_receivers(lines: list[str]) -> set[str]:
    receivers = set(PG_CONTEXT_TYPES)
    code = "\n".join(line.split("//", 1)[0] for line in lines)
    for use_statement in USE_STATEMENT_RE.finditer(code):
        for alias_match in PG_CONTEXT_USE_ALIAS_RE.finditer(use_statement.group(0)):
            receivers.add(alias_match.group("alias"))
    for alias_match in PG_CONTEXT_TYPE_ALIAS_RE.finditer(code):
        receivers.add(alias_match.group("alias"))
    return receivers


def legacy_function_aliases(lines: list[str]) -> dict[str, str]:
    aliases: dict[str, str] = {}
    code = "\n".join(line.split("//", 1)[0] for line in lines)
    for use_statement in USE_STATEMENT_RE.finditer(code):
        for alias_match in LEGACY_FUNCTION_ALIAS_RE.finditer(use_statement.group(0)):
            aliases[alias_match.group("alias")] = alias_match.group("function")
    return aliases


def balanced_parenthesized(text: str, open_index: int) -> str | None:
    depth = 0
    content_start = open_index + 1
    quote: str | None = None
    escaped = False
    for index in range(open_index, len(text)):
        char = text[index]
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {'"', "'"}:
            quote = char
            continue
        if char == "(":
            depth += 1
            continue
        if char == ")":
            depth -= 1
            if depth == 0:
                return text[content_start:index]
    return None


def split_top_level_args(expression: str) -> list[str]:
    args: list[str] = []
    depth = 0
    start = 0
    quote: str | None = None
    escaped = False
    for index, char in enumerate(expression):
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {'"', "'"}:
            quote = char
            continue
        if char == "(":
            depth += 1
            continue
        if char == ")":
            depth = max(0, depth - 1)
            continue
        if char == "," and depth == 0:
            args.append(expression[start:index].strip())
            start = index + 1
    args.append(expression[start:].strip())
    return [arg for arg in args if arg]


def cfg_call(expression: str) -> tuple[str, str] | None:
    expression = expression.strip()
    match = re.match(r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\(", expression)
    if not match:
        return None
    args = balanced_parenthesized(expression, match.end() - 1)
    if args is None:
        return None
    return match.group("name"), args


def cfg_expression_is_test_only(expression: str) -> bool:
    expression = expression.strip()
    if expression == "test":
        return True
    call = cfg_call(expression)
    if not call:
        return False
    name, args = call
    return name == "all" and any(arg.strip() == "test" for arg in split_top_level_args(args))


def is_test_only_cfg(code: str) -> bool:
    for match in CFG_START_RE.finditer(code):
        expression = balanced_parenthesized(code, match.end() - 1)
        if expression is not None and cfg_expression_is_test_only(expression):
            return True
    return False


def is_pg_context_receiver(receiver: str, context_receivers: set[str]) -> bool:
    return receiver_type_name(receiver) in context_receivers


def is_allowed_legacy_context(
    current_function: str | None,
    current_impl: str | None,
    in_test_block: bool,
    store_receivers: set[str],
) -> bool:
    if in_test_block:
        return True
    if current_function and GLOBAL_ALLOWED_LEGACY_FUNCTIONS.match(current_function):
        return True
    if current_function in {"open", "open_with_database_url"}:
        return bool(current_impl and current_impl in store_receivers)
    if current_function == "store_context":
        return current_impl == "PostgresBackend"
    return False


def scan_file(path: Path) -> list[Violation]:
    if is_test_source(path):
        return []

    lines = path.read_text(encoding="utf-8").splitlines()
    violations: list[Violation] = []
    store_receivers = legacy_store_receivers(lines)
    context_receivers = pg_context_receivers(lines)
    function_aliases = legacy_function_aliases(lines)

    depth = 0
    cfg_test_depths: set[int] = set()
    pending_cfg_test = False
    pending_function: str | None = None
    pending_impl: str | None = None
    function_depths: dict[int, str] = {}
    impl_depths: dict[int, str] = {}

    for index, line in enumerate(lines):
        code = line.split("//", 1)[0]
        fn_match = FN_RE.search(code)
        if fn_match:
            pending_function = fn_match.group(1)
        impl_match = impl_type_name(code)
        if impl_match:
            pending_impl = impl_match
        if is_test_only_cfg(code):
            pending_cfg_test = True

        open_braces = code.count("{")
        close_braces = code.count("}")
        new_depth = depth
        for _ in range(open_braces):
            new_depth += 1
            if pending_cfg_test:
                cfg_test_depths.add(new_depth)
                pending_cfg_test = False
            if pending_function:
                function_depths[new_depth] = pending_function
                pending_function = None
            if pending_impl:
                impl_depths[new_depth] = pending_impl
                pending_impl = None

        cfg_test_depths = {test_depth for test_depth in cfg_test_depths if test_depth <= new_depth}
        current_function = function_depths.get(max(function_depths, default=0))
        current_impl = impl_depths.get(max(impl_depths, default=0))
        in_test_block = any(new_depth >= test_depth for test_depth in cfg_test_depths)
        is_allowed = is_allowed_legacy_context(
            current_function,
            current_impl,
            in_test_block,
            store_receivers,
        )

        if is_comment_or_blank(line):
            for _ in range(close_braces):
                new_depth = max(0, new_depth - 1)
            function_depths = {
                function_depth: function_name
                for function_depth, function_name in function_depths.items()
                if function_depth <= new_depth
            }
            cfg_test_depths = {
                test_depth for test_depth in cfg_test_depths if test_depth <= new_depth
            }
            impl_depths = {
                impl_depth: impl_name
                for impl_depth, impl_name in impl_depths.items()
                if impl_depth <= new_depth
            }
            if code.strip().endswith(";"):
                pending_cfg_test = False
            depth = new_depth
            continue
        for pattern in LEGACY_PATTERNS:
            if pattern not in line:
                continue
            if is_allowed:
                continue
            violations.append(Violation(path, index + 1, pattern, line))
        for alias, function_name in function_aliases.items():
            if not re.search(rf"\b{re.escape(alias)}\s*\(", line):
                continue
            if is_allowed:
                continue
            violations.append(Violation(path, index + 1, f"{function_name}(", line))
        for context_match in PG_CONTEXT_LEGACY_RE.finditer(line):
            if not is_pg_context_receiver(
                context_match.group("receiver"),
                context_receivers,
            ):
                continue
            if is_allowed:
                continue
            violations.append(
                Violation(path, index + 1, "PgStoreContext::from_legacy_path_schema", line)
            )
        for store_open_match in LEGACY_STORE_OPEN_RE.finditer(line):
            receiver = store_open_match.group("receiver")
            is_legacy_receiver = (
                receiver == "Self" and current_impl and current_impl in store_receivers
            ) or receiver_type_name(receiver) in store_receivers
            if not is_legacy_receiver:
                continue
            pattern = f"known-store::{store_open_match.group('method')}("
            if is_allowed:
                continue
            violations.append(Violation(path, index + 1, pattern, line))
        for _ in range(close_braces):
            new_depth = max(0, new_depth - 1)
        function_depths = {
            function_depth: function_name
            for function_depth, function_name in function_depths.items()
            if function_depth <= new_depth
        }
        cfg_test_depths = {
            test_depth for test_depth in cfg_test_depths if test_depth <= new_depth
        }
        impl_depths = {
            impl_depth: impl_name
            for impl_depth, impl_name in impl_depths.items()
            if impl_depth <= new_depth
        }
        if code.strip().endswith(";"):
            pending_cfg_test = False
        depth = new_depth
    return violations


def iter_rust_sources(root: Path) -> list[Path]:
    crates_dir = root / "crates"
    if not crates_dir.is_dir():
        return []
    return sorted(crates_dir.glob("**/*.rs"))


def scan(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for path in iter_rust_sources(root):
        violations.extend(scan_file(path))
    return violations


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(dedent(content).lstrip(), encoding="utf-8")


def run_self_test() -> int:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        write(
            root / "crates/demo/src/lib.rs",
            """
            use harness_core::db::PgStoreContext;
            use harness_core::db::PgStoreContext as ContextAlias;
            use harness_core::db::{
                PgStoreContext as GroupedContextAlias,
                legacy_schema_for_path as schema_alias,
                open_legacy_path_schema_pool as pool_alias,
            };
            use harness_workflow::runtime::WorkflowRuntimeStore as StoreAlias;

            type ContextTypeAlias = PgStoreContext;

            pub fn disallowed(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }

            pub fn disallowed_context_alias(path: &std::path::Path) {
                let _ = ContextAlias::from_legacy_path_schema(path, None);
            }

            pub fn disallowed_grouped_context_alias(path: &std::path::Path) {
                let _ = GroupedContextAlias::from_legacy_path_schema(path, None);
            }

            pub fn disallowed_context_type_alias(path: &std::path::Path) {
                let _ = ContextTypeAlias::from_legacy_path_schema(path, None);
            }

            pub async fn disallowed_runtime(path: &std::path::Path) {
                let _ = WorkflowRuntimeStore::open_with_database_url(path, None).await;
            }

            pub fn disallowed_schema(path: &std::path::Path) {
                let _ = legacy_schema_for_path(path);
            }

            pub fn open(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }

            pub fn disallowed_schema_alias(path: &std::path::Path) {
                let _ = schema_alias(path);
            }

            pub fn disallowed_pool_alias(path: &std::path::Path) {
                let _ = pool_alias(path);
            }

            pub async fn disallowed_alias(path: &std::path::Path) {
                let _ = StoreAlias::open_with_database_url(path, None).await;
            }

            use harness_workflow::runtime::{
                WorkflowRuntimeStore as GroupedStoreAlias,
            };

            pub async fn disallowed_grouped_alias(path: &std::path::Path) {
                let _ = GroupedStoreAlias::open_with_database_url(path, None).await;
            }

            pub async fn disallowed_open_wrapper(path: &std::path::Path) {
                let _ = WorkflowRuntimeStore::open(path).await;
            }

            pub async fn disallowed_mixed_open_line(path: &std::path::Path) {
                let _ = (File::open(path), WorkflowRuntimeStore::open(path).await);
            }

            impl WorkflowRuntimeStore {
                pub async fn reconnect(path: &std::path::Path) {
                    let _ = Self::open_with_database_url(path, None).await;
                }

                pub async fn open(path: &std::path::Path) {
                    let _ = Self::open_with_database_url(path, None).await;
                }
            }

            impl ExternalClient {
                pub async fn reconnect(path: &std::path::Path) {
                    let _ = Self::open_with_database_url(path, None).await;
                }
            }

            pub async fn migrate_legacy_demo_if_needed(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
                let _ = WorkflowRuntimeStore::open_with_database_url(path, None).await;
                let _ = WorkflowRuntimeStore::open(path).await;
                let _ = legacy_schema_for_path(path);
            }

            pub async fn migrate_project_workflows_if_needed(path: &std::path::Path) {
                let _ = ProjectWorkflowStore::open_with_database_url(path, None).await;
            }

            pub fn legacy_schema_for_path(path: &std::path::Path) -> anyhow::Result<String> {
                Ok(path.display().to_string())
            }
            """,
        )
        write(
            root / "crates/demo/src/demo_tests.rs",
            """
            use harness_core::db::PgStoreContext;

            pub fn allowed_test(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }
            """,
        )
        write(
            root / "crates/demo/src/cfg_test.rs",
            """
            use harness_core::db::PgStoreContext;

            #[cfg(test)]
            mod tests {
                use super::*;

                pub fn allowed_cfg_test(path: &std::path::Path) {
                    let _ = PgStoreContext::from_legacy_path_schema(path, None);
                }
            }

            pub fn disallowed_after_cfg_test(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }

            #[cfg(any(test, target_os = "linux"))]
            pub fn disallowed_cfg_any_test_and_prod(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }

            #[cfg(all(feature = "test-support"))]
            pub fn disallowed_cfg_feature_test_support(path: &std::path::Path) {
                let _ = PgStoreContext::from_legacy_path_schema(path, None);
            }

            #[cfg(all(test, target_os = "linux"))]
            mod linux_tests {
                use super::*;

                pub fn allowed_cfg_all_test(path: &std::path::Path) {
                    let _ = PgStoreContext::from_legacy_path_schema(path, None);
                }
            }

            #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
            mod nested_linux_tests {
                use super::*;

                pub fn allowed_cfg_nested_all_test(path: &std::path::Path) {
                    let _ = PgStoreContext::from_legacy_path_schema(path, None);
                }
            }
            """,
        )

        violations = scan(root)
        if len(violations) != 17:
            sys.stderr.write(
                f"self-test failed: expected 17 violations, found {len(violations)}\n"
            )
            for violation in violations:
                sys.stderr.write(f"{violation.format(root)}\n")
            return 1
        pattern_counts = {
            pattern: sum(1 for violation in violations if violation.pattern == pattern)
            for pattern in {violation.pattern for violation in violations}
        }
        expected_pattern_counts = {
            "PgStoreContext::from_legacy_path_schema": 8,
            "legacy_schema_for_path(": 2,
            "open_legacy_path_schema_pool(": 1,
            "known-store::open_with_database_url(": 4,
            "known-store::open(": 2,
        }
        if pattern_counts != expected_pattern_counts:
            sys.stderr.write(f"self-test failed: unexpected violations {violations}\n")
            return 1

    sys.stdout.write("storage legacy opener guard self-test passed\n")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Detect disallowed production uses of legacy path-derived storage openers."
    )
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="repository root to scan")
    parser.add_argument("--self-test", action="store_true", help="run the scanner self-test")
    args = parser.parse_args()

    if args.self_test:
        return run_self_test()

    root = args.root.resolve()
    violations = scan(root)
    if violations:
        sys.stderr.write("Disallowed legacy path-derived storage opener usage found:\n")
        for violation in violations:
            sys.stderr.write(f"{violation.format(root)}\n")
        sys.stderr.write(
            "\nUse shared-schema store contexts in production paths. Legacy path-derived "
            "openers are allowed only in explicit migration/backfill APIs, compatibility "
            "constructor definitions, backend seam code, and tests.\n"
        )
        return 1

    sys.stdout.write("storage legacy opener guard passed\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
