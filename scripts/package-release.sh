#!/usr/bin/env bash
# Package a built harness binary as a release archive plus SHA256 checksum.
#
# Usage: scripts/package-release.sh <target-triple> <binary-path> <out-dir>
# Produces <out-dir>/harness-<target>.tar.gz and <out-dir>/harness-<target>.tar.gz.sha256.
# The archive holds harness-<target>/{harness,LICENSE,README.md}.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <target-triple> <binary-path> <out-dir>" >&2
  exit 2
fi

target="$1"
binary="$2"
out_dir="$3"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="harness-${target}"

[[ -x "$binary" ]] || { echo "ERROR: $binary is not an executable file" >&2; exit 2; }

mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd)"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT

mkdir "$staging/$name"
cp "$binary" "$staging/$name/harness"
cp "$root/LICENSE" "$root/README.md" "$staging/$name/"
tar -C "$staging" -czf "$out_dir/$name.tar.gz" "$name"

cd "$out_dir"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
fi
cat "$name.tar.gz.sha256"
