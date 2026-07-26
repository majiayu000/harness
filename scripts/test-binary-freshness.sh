#!/usr/bin/env bash
# Regression tests for the GH-1763 binary freshness contract:
# scripts/lib/binary-freshness.sh plus its integration in
# scripts/start-harness-codex-safe.sh.
#
# Covers: fresh, stale (newer input / removed input / newer manifest),
# missing evidence, malformed evidence, escaped-space locators, explicit
# operator-owned binary override, refusal of a stale default binary, and
# freshness reporting on the recorded-live-PID path.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/binary-freshness.sh
. "$REPO_ROOT/scripts/lib/binary-freshness.sh"

FAILURES=0

pass() {
    printf 'PASS: %s\n' "$1"
}

fail() {
    printf 'FAIL: %s\n' "$1" >&2
    FAILURES=$((FAILURES + 1))
}

OLD_TS=202601010000
NEW_TS=202601020000

FIXTURE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/harness-binary-freshness.XXXXXX")"
trap 'rm -rf "$FIXTURE_ROOT"' EXIT

# new_fixture NAME -> prints the fixture repo path
# Layout: Cargo.toml, Cargo.lock, src/main.rs, target/release/harness{,.d},
# everything older than the binary, so the baseline state is fresh.
new_fixture() {
    local fix="$FIXTURE_ROOT/$1"
    mkdir -p "$fix/src" "$fix/target/release"
    printf '[package]\nname = "fixture"\n' > "$fix/Cargo.toml"
    printf '# lock\n' > "$fix/Cargo.lock"
    printf 'fn main() {}\n' > "$fix/src/main.rs"
    printf '#!/bin/sh\nexit 0\n' > "$fix/target/release/harness"
    chmod +x "$fix/target/release/harness"
    printf '%s/target/release/harness: %s/src/main.rs\n' "$fix" "$fix" \
        > "$fix/target/release/harness.d"
    touch -t "$OLD_TS" "$fix/Cargo.toml" "$fix/Cargo.lock" "$fix/src/main.rs" \
        "$fix/target/release/harness.d"
    touch -t "$NEW_TS" "$fix/target/release/harness"
    printf '%s\n' "$fix"
}

expect_state() {
    local label="$1" expected="$2" fix="$3"
    local actual printed
    printed="$(harness_binary_freshness "$fix/target/release/harness" "$fix")"
    harness_binary_freshness "$fix/target/release/harness" "$fix" > /dev/null
    actual="$HARNESS_BINARY_FRESHNESS_STATE"
    if [ "$printed" != "$actual" ]; then
        fail "$label: printed state '$printed' != variable state '$actual'"
        return 0
    fi
    if [ "$actual" = "$expected" ]; then
        pass "$label -> $actual"
    else
        fail "$label: expected $expected, got $actual (detail: $HARNESS_BINARY_FRESHNESS_DETAIL)"
    fi
}

# --- helper unit cases ---------------------------------------------------

fix="$(new_fixture fresh)"
expect_state "fresh binary" fresh "$fix"

fix="$(new_fixture stale-newer-input)"
touch "$fix/src/main.rs"
expect_state "build input newer than binary" stale "$fix"

fix="$(new_fixture stale-removed-input)"
rm "$fix/src/main.rs"
expect_state "build input removed since build" stale "$fix"

fix="$(new_fixture stale-newer-manifest)"
touch "$fix/Cargo.lock"
expect_state "root manifest newer than binary" stale "$fix"

fix="$(new_fixture missing-evidence)"
rm "$fix/target/release/harness.d"
expect_state "missing dep-info evidence" unverifiable "$fix"

fix="$(new_fixture malformed-evidence)"
printf 'not a dep-info line\n' > "$fix/target/release/harness.d"
touch -t "$OLD_TS" "$fix/target/release/harness.d"
expect_state "malformed dep-info evidence" unverifiable "$fix"

fix="$(new_fixture empty-deps)"
printf '%s/target/release/harness:\n' "$fix" > "$fix/target/release/harness.d"
touch -t "$OLD_TS" "$fix/target/release/harness.d"
expect_state "dep-info with no repository inputs" unverifiable "$fix"

fix="$(new_fixture escaped-space)"
mkdir -p "$fix/src/with space"
printf 'fn main() {}\n' > "$fix/src/with space/lib.rs"
printf '%s/target/release/harness: %s/src/with\\ space/lib.rs\n' "$fix" "$fix" \
    > "$fix/target/release/harness.d"
touch -t "$OLD_TS" "$fix/src/with space/lib.rs" "$fix/target/release/harness.d"
expect_state "escaped-space build input (fresh)" fresh "$fix"
touch "$fix/src/with space/lib.rs"
expect_state "escaped-space build input (stale)" stale "$fix"

fix="$(new_fixture missing-manifest)"
rm "$fix/Cargo.lock"
expect_state "missing root manifest" unverifiable "$fix"

# --- start-harness-codex-safe.sh integration cases -----------------------

STARTER="$REPO_ROOT/scripts/start-harness-codex-safe.sh"

if ! command -v lsof > /dev/null 2>&1; then
    echo "SKIP: lsof unavailable; starter integration cases not run" >&2
else
    # A stale default binary must be refused before any launch.
    fix="$(new_fixture starter-stale-default)"
    touch "$fix/src/main.rs"
    set +e
    output="$(cd "$fix" && "$STARTER" --port 39411 2>&1)"
    status=$?
    set -e
    if [ "$status" -eq 3 ] &&
        printf '%s' "$output" | grep -q "refusing to start harness server: default binary is stale"; then
        pass "starter refuses stale default binary (exit 3)"
    else
        fail "starter stale-default refusal: exit=$status output=$output"
    fi

    # An unverifiable default binary must also be refused.
    fix="$(new_fixture starter-unverifiable-default)"
    rm "$fix/target/release/harness.d"
    set +e
    output="$(cd "$fix" && "$STARTER" --port 39412 2>&1)"
    status=$?
    set -e
    if [ "$status" -eq 3 ] &&
        printf '%s' "$output" | grep -q "refusing to start harness server: default binary is unverifiable"; then
        pass "starter refuses unverifiable default binary (exit 3)"
    else
        fail "starter unverifiable-default refusal: exit=$status output=$output"
    fi

    # Explicit --bin is operator-owned: warn but run.
    fix="$(new_fixture starter-explicit-bin)"
    printf '#!/bin/sh\nexit 0\n' > "$fix/operator-harness"
    chmod +x "$fix/operator-harness"
    set +e
    output="$(cd "$fix" && "$STARTER" --port 39413 --bin "$fix/operator-harness" --foreground 2>&1)"
    status=$?
    set -e
    if [ "$status" -eq 0 ] &&
        printf '%s' "$output" | grep -q "warning: operator-selected binary is" &&
        printf '%s' "$output" | grep -q "freshness is not enforced"; then
        pass "starter allows explicit operator-owned binary with warning"
    else
        fail "starter explicit-bin override: exit=$status output=$output"
    fi

    # The recorded-live-PID fast path reports freshness separately from
    # process health and stays successful and non-destructive.
    fix="$(new_fixture starter-live-pid)"
    touch "$fix/src/main.rs"
    mkdir -p "$fix/.harness/local"
    printf '%s\n' "$$" > "$fix/.harness/local/harness-39414.pid"
    set +e
    output="$(cd "$fix" && "$STARTER" --port 39414 2>&1)"
    status=$?
    set -e
    if [ "$status" -eq 0 ] &&
        printf '%s' "$output" | grep -q "already recorded as running" &&
        printf '%s' "$output" | grep -q "binary_freshness=stale" &&
        printf '%s' "$output" | grep -q "independent of process health"; then
        pass "starter live-PID path reports freshness separately from health"
    else
        fail "starter live-PID freshness report: exit=$status output=$output"
    fi

    # --status reports freshness for a live recorded PID.
    set +e
    output="$(cd "$fix" && "$STARTER" --port 39414 --status 2>&1)"
    status=$?
    set -e
    if [ "$status" -eq 0 ] &&
        printf '%s' "$output" | grep -q "binary_freshness=stale"; then
        pass "starter --status reports binary freshness"
    else
        fail "starter --status freshness report: exit=$status output=$output"
    fi
fi

if [ "$FAILURES" -gt 0 ]; then
    printf '%d test(s) failed\n' "$FAILURES" >&2
    exit 1
fi
echo "all binary freshness tests passed"
