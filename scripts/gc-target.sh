#!/usr/bin/env bash
# Manually clean stale Cargo build artifacts from repository-local target dirs.
set -euo pipefail

DEFAULT_DAYS=14
DAYS="$DEFAULT_DAYS"
DRY_RUN=0

usage() {
  cat <<'EOF'
Usage: scripts/gc-target.sh [--days N] [--dry-run]

Manually removes stale Cargo build artifacts from repository-local target
directories. The default retention window is 14 days.

Options:
  --days N    Keep artifacts newer than N days. N must be a non-negative integer.
  --dry-run   Print stale artifact candidates without deleting them.
  -h, --help  Show this help text.

The script prefers cargo sweep when it is installed. Without cargo sweep, it
falls back to mtime-based cleanup under target profile artifact directories.
Do not run it while Cargo builds are active in this repository.
EOF
}

fail() {
  echo "error: $*" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --days)
      [ "$#" -ge 2 ] || fail "--days requires a value"
      DAYS="$2"
      shift 2
      ;;
    --days=*)
      DAYS="${1#--days=}"
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

if [[ ! "$DAYS" =~ ^[0-9]+$ ]]; then
  fail "--days must be a non-negative integer"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_ROOT="$REPO_ROOT/target"

TARGET_UNIVERSES=()
if [ -d "$TARGET_ROOT" ]; then
  TARGET_UNIVERSES+=("$TARGET_ROOT")
  for universe in "$TARGET_ROOT"/cargo-*; do
    [ -d "$universe" ] || continue
    TARGET_UNIVERSES+=("$universe")
  done
fi

if [ "${#TARGET_UNIVERSES[@]}" -eq 0 ]; then
  echo "No repository target directories found under $TARGET_ROOT; nothing to clean."
  exit 0
fi

relative_path() {
  local path="$1"
  if [[ "$path" == "$REPO_ROOT/"* ]]; then
    printf '%s\n' "${path#"$REPO_ROOT"/}"
  else
    printf '%s\n' "$path"
  fi
}

dir_size_human() {
  local path="$1"
  du -sh "$path" 2>/dev/null | awk '{print $1}'
}

dir_size_bytes() {
  local path="$1"
  if [ ! -d "$path" ]; then
    echo 0
    return
  fi
  du -sk "$path" 2>/dev/null | awk '{print $1 * 1024}'
}

print_sizes() {
  local label="$1"
  local universe size

  echo "$label target sizes:"
  for universe in "${TARGET_UNIVERSES[@]}"; do
    [ -d "$universe" ] || continue
    size="$(dir_size_human "$universe")"
    echo "  $(relative_path "$universe"): $size"
  done
}

has_cargo_sweep() {
  command -v cargo >/dev/null 2>&1 && cargo sweep --help >/dev/null 2>&1
}

run_cargo_sweep_default_target() {
  echo "Using cargo sweep for target with retention ${DAYS}d."
  (
    cd "$REPO_ROOT"
    cargo sweep --time "$DAYS"
  )
}

FALLBACK_CANDIDATES=0
FALLBACK_REMOVED=0

cleanup_artifact_dir() {
  local artifact_dir="$1"
  local candidates path count

  [ -d "$artifact_dir" ] || return 0

  candidates="$(mktemp "${TMPDIR:-/tmp}/harness-gc-target.XXXXXX")"
  if ! find "$artifact_dir" -depth -mindepth 1 -mtime +"$DAYS" -print0 > "$candidates"; then
    rm -f "$candidates"
    echo "failed to scan $(relative_path "$artifact_dir")" >&2
    return 1
  fi

  count=0
  while IFS= read -r -d '' path; do
    count=$((count + 1))
    if [ "$DRY_RUN" -eq 1 ]; then
      echo "  would remove $(relative_path "$path")"
    elif [ -e "$path" ] || [ -L "$path" ]; then
      rm -rf -- "$path"
      FALLBACK_REMOVED=$((FALLBACK_REMOVED + 1))
    fi
  done < "$candidates"
  rm -f "$candidates"

  FALLBACK_CANDIDATES=$((FALLBACK_CANDIDATES + count))
}

run_fallback_for_universe() {
  local universe="$1"
  local name profile base

  echo "Scanning $(relative_path "$universe") with fallback mtime cleanup."

  for name in deps incremental build; do
    cleanup_artifact_dir "$universe/$name"
  done

  for profile in "$universe"/*; do
    [ -d "$profile" ] || continue
    base="${profile##*/}"
    if [ "$universe" = "$TARGET_ROOT" ] && [[ "$base" == cargo-* ]]; then
      continue
    fi
    for name in deps incremental build; do
      cleanup_artifact_dir "$profile/$name"
    done
  done
}

run_fallback_cleanup() {
  local universe

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Dry run: reporting stale fallback cleanup candidates older than ${DAYS}d."
  else
    echo "Using fallback mtime cleanup for artifacts older than ${DAYS}d."
  fi

  for universe in "${TARGET_UNIVERSES[@]}"; do
    run_fallback_for_universe "$universe"
  done

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Dry run candidates: $FALLBACK_CANDIDATES"
  else
    echo "Fallback removed paths: $FALLBACK_REMOVED"
  fi
}

print_sizes "Before"
BEFORE_BYTES="$(dir_size_bytes "$TARGET_ROOT")"

if [ "$DRY_RUN" -eq 1 ]; then
  run_fallback_cleanup
elif has_cargo_sweep; then
  run_cargo_sweep_default_target
  for universe in "${TARGET_UNIVERSES[@]}"; do
    [ "$universe" != "$TARGET_ROOT" ] || continue
    run_fallback_for_universe "$universe"
  done
else
  run_fallback_cleanup
fi

AFTER_BYTES="$(dir_size_bytes "$TARGET_ROOT")"
print_sizes "After"

if [ "$BEFORE_BYTES" -gt "$AFTER_BYTES" ]; then
  FREED_BYTES=$((BEFORE_BYTES - AFTER_BYTES))
else
  FREED_BYTES=0
fi

echo "Freed: ${FREED_BYTES} bytes"
