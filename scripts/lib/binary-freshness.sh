# shellcheck shell=bash
# Shared read-only freshness contract for repository-local default Harness
# binaries (target/release/harness, target/debug/harness).
#
# Sourced by scripts/start-harness-codex-safe.sh, start-server.sh, and
# scripts/doctor.sh. This file must stay side-effect free: it never builds,
# writes, deletes, or starts anything.
#
# Contract (GH-1763):
# - Evidence is the Cargo dep-info file written next to the binary
#   ("<binary>.d") plus the repository root Cargo.toml and Cargo.lock.
# - A binary is "fresh" when every repository-local build input listed in the
#   dep-info file, and both root manifests, are not newer than the binary.
# - A listed repository build input that is missing or newer than the binary
#   makes the binary "stale" (sources changed since the build).
# - Missing, unreadable, or malformed evidence is "unverifiable" (fail
#   closed): it is never reported as fresh.
# - Out-of-repository inputs (registry sources, toolchain) are intentionally
#   not scanned; their changes are visible through Cargo.lock.

# harness_binary_freshness BIN REPO_ROOT
#   Prints exactly one of: fresh | stale | unverifiable
#   Also sets HARNESS_BINARY_FRESHNESS_STATE to that value and
#   HARNESS_BINARY_FRESHNESS_DETAIL to a one-line reason. Callers should
#   invoke the function directly (not in command substitution) and read the
#   variables, so the detail survives; the printed value exists for ad hoc
#   shell use. Always returns 0 so `set -e` callers can branch on the state.
# shellcheck disable=SC2034  # read by the scripts that source this library
HARNESS_BINARY_FRESHNESS_STATE=""
HARNESS_BINARY_FRESHNESS_DETAIL=""

_harness_bf_mtime() {
    stat -f %m -- "$1" 2>/dev/null || stat -c %Y -- "$1" 2>/dev/null
}

# POSIX treats repeated slashes as one separator; squeeze them so textual
# prefix matching cannot be defeated by "dir//file" spellings in dep-info.
_harness_bf_squeeze_slashes() {
    # Named pattern/replacement variables: literal backslash-escaped slashes
    # in ${var//pattern/replacement} misparse under macOS stock bash 3.2.
    local path="$1" double_slash='//' single_slash='/'
    while case "$path" in *//*) true ;; *) false ;; esac; do
        path="${path//$double_slash/$single_slash}"
    done
    printf '%s\n' "$path"
}

harness_binary_freshness() {
    HARNESS_BINARY_FRESHNESS_STATE=""
    HARNESS_BINARY_FRESHNESS_DETAIL=""
    local bin="$1"
    local root="$2"
    local depinfo="${bin}.d"
    local bin_mtime="" dep_mtime="" line="" deps="" dep=""
    local checked=0 had_noglob=0

    root="$(_harness_bf_squeeze_slashes "$root")"

    if [ ! -f "$bin" ]; then
        HARNESS_BINARY_FRESHNESS_DETAIL="binary not found: $bin"
        HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
        return 0
    fi
    bin_mtime="$(_harness_bf_mtime "$bin" || true)"
    if [ -z "$bin_mtime" ]; then
        HARNESS_BINARY_FRESHNESS_DETAIL="cannot read binary mtime: $bin"
        HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
        return 0
    fi
    if [ ! -f "$depinfo" ]; then
        HARNESS_BINARY_FRESHNESS_DETAIL="missing freshness evidence: $depinfo"
        HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
        return 0
    fi
    line="$(head -n 1 -- "$depinfo" 2>/dev/null || true)"
    case "$line" in
        *:*) ;;
        *)
            HARNESS_BINARY_FRESHNESS_DETAIL="malformed freshness evidence: $depinfo"
            HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
            return 0
            ;;
    esac
    deps="${line#*:}"
    # Cargo escapes spaces in paths as "\ "; protect them across word split.
    deps="${deps//\\ /$'\x1f'}"

    case $- in *f*) had_noglob=1 ;; esac
    set -f
    for dep in $deps; do
        dep="${dep//$'\x1f'/ }"
        [ -n "$dep" ] || continue
        dep="$(_harness_bf_squeeze_slashes "$dep")"
        case "$dep" in
            "$root"/*) ;;
            *) continue ;;
        esac
        if [ ! -e "$dep" ]; then
            [ "$had_noglob" -eq 1 ] || set +f
            HARNESS_BINARY_FRESHNESS_DETAIL="build input removed or renamed since build: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=stale && printf 'stale\n'
            return 0
        fi
        dep_mtime="$(_harness_bf_mtime "$dep" || true)"
        if [ -z "$dep_mtime" ]; then
            [ "$had_noglob" -eq 1 ] || set +f
            HARNESS_BINARY_FRESHNESS_DETAIL="cannot read build input mtime: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
            return 0
        fi
        if [ "$dep_mtime" -gt "$bin_mtime" ]; then
            [ "$had_noglob" -eq 1 ] || set +f
            HARNESS_BINARY_FRESHNESS_DETAIL="build input newer than binary: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=stale && printf 'stale\n'
            return 0
        fi
        checked=$((checked + 1))
    done
    [ "$had_noglob" -eq 1 ] || set +f

    if [ "$checked" -eq 0 ]; then
        HARNESS_BINARY_FRESHNESS_DETAIL="no repository build inputs listed in $depinfo"
        HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
        return 0
    fi

    for dep in "$root/Cargo.toml" "$root/Cargo.lock"; do
        if [ ! -f "$dep" ]; then
            HARNESS_BINARY_FRESHNESS_DETAIL="missing root manifest: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
            return 0
        fi
        dep_mtime="$(_harness_bf_mtime "$dep" || true)"
        if [ -z "$dep_mtime" ]; then
            HARNESS_BINARY_FRESHNESS_DETAIL="cannot read root manifest mtime: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=unverifiable && printf 'unverifiable\n'
            return 0
        fi
        if [ "$dep_mtime" -gt "$bin_mtime" ]; then
            HARNESS_BINARY_FRESHNESS_DETAIL="root manifest newer than binary: $dep"
            HARNESS_BINARY_FRESHNESS_STATE=stale && printf 'stale\n'
            return 0
        fi
    done

    HARNESS_BINARY_FRESHNESS_DETAIL="verified against $checked repository build inputs and root manifests"
    HARNESS_BINARY_FRESHNESS_STATE=fresh && printf 'fresh\n'
    return 0
}

# harness_binary_rebuild_hint BIN
#   Prints the actionable rebuild command for a repository-local binary.
harness_binary_rebuild_hint() {
    case "$1" in
        */target/debug/*)
            printf 'cargo build -p harness-cli\n'
            ;;
        *)
            printf 'cargo build --release -p harness-cli\n'
            ;;
    esac
}
