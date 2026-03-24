#!/usr/bin/env bash
# verify-package-contents.sh — pre-publish tarball smoke test
# Packs the package into a temporary tarball and checks that all required
# files are present before allowing `npm publish` to proceed.
set -euo pipefail

REQUIRED_FILES=(
  "package/dist/index.js"
  "package/dist/index.cjs"
  "package/dist/index.d.ts"
)

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Packing tarball for verification..."
npm pack --pack-destination "$TMPDIR" --quiet

TARBALL=$(ls "$TMPDIR"/*.tgz | head -1)
if [ -z "$TARBALL" ]; then
  echo "ERROR: npm pack produced no tarball" >&2
  exit 1
fi

echo "Unpacking $TARBALL..."
tar -xzf "$TARBALL" -C "$TMPDIR"

MISSING=0
for f in "${REQUIRED_FILES[@]}"; do
  if [ ! -f "$TMPDIR/$f" ]; then
    echo "MISSING: $f" >&2
    MISSING=1
  else
    echo "  OK: $f"
  fi
done

if [ "$MISSING" -ne 0 ]; then
  echo ""
  echo "ERROR: npm publish aborted — required files are missing from the tarball." >&2
  echo "Run 'npm run build' to regenerate dist/, then retry." >&2
  exit 1
fi

echo ""
echo "All required files present in tarball."
