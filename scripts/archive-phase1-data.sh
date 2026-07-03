#!/usr/bin/env bash
set -euo pipefail
umask 077

die() {
  echo "ERROR: $*" >&2
  exit 2
}

need_tool() {
  local env_name="$1"
  local name="$2"
  local fallback="/opt/homebrew/opt/libpq/bin/$2"
  if [[ -n "${!env_name:-}" ]]; then
    printf '%s\n' "${!env_name}"
    return
  fi
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return
  fi
  if [[ -x "$fallback" ]]; then
    printf '%s\n' "$fallback"
    return
  fi
  die "$name not found; set $env_name"
}

[[ -n "${HARNESS_DATABASE_URL:-}" ]] || die "HARNESS_DATABASE_URL is required"

PSQL="$(need_tool PSQL psql)"
PG_DUMP="$(need_tool PG_DUMP pg_dump)"
PG_RESTORE="$(need_tool PG_RESTORE pg_restore)"
ARCHIVE_ROOT="${ARCHIVE_ROOT:-archives}"
ARCHIVE_DATE="${ARCHIVE_DATE:-$(date -u +%Y%m%dT%H%M%SZ)}"
OUT_DIR="${ARCHIVE_DIR:-$ARCHIVE_ROOT/phase1-$ARCHIVE_DATE}"
LOCK_WAIT_TIMEOUT="${PG_DUMP_LOCK_WAIT_TIMEOUT:-5s}"

mkdir -p "$ARCHIVE_ROOT"
[[ ! -e "$OUT_DIR" ]] || die "archive already exists: $OUT_DIR"
mkdir "$OUT_DIR"

psql_cmd=("$PSQL" -XAt --set ON_ERROR_STOP=1 "$HARNESS_DATABASE_URL")
TABLE_SQL="
SELECT table_schema || '.' || table_name
FROM information_schema.tables
WHERE table_type = 'BASE TABLE'
  AND (
    (
      table_schema IN ('thread_db', 'task_db', 'eval_store', 'review_store')
      AND table_name IN (
        'schema_migrations', 'threads', 'tasks', 'task_artifacts',
        'task_checkpoints', 'task_prompts', 'workspace_leases',
        'eval_runs', 'eval_artifacts', 'quality_snapshots', 'review_findings'
      )
    )
    OR (
      table_schema ~ '^h[0-9a-f]{16}$'
      AND table_name IN (
        'schema_migrations', 'threads', 'tasks', 'task_artifacts',
        'task_checkpoints', 'task_prompts', 'workspace_leases',
        'eval_runs', 'eval_artifacts', 'quality_snapshots', 'review_findings'
      )
    )
  )
ORDER BY table_schema, table_name"

tables_file="$OUT_DIR/tables.txt"
if ! "${psql_cmd[@]}" -c "$TABLE_SQL" > "$tables_file"; then
  die "failed to query phase1 tables"
fi
mapfile -t tables < "$tables_file"
[[ "${#tables[@]}" -gt 0 ]] || die "no phase1 tables found"
required_tables=(thread_db.threads task_db.tasks)
for required_table in "${required_tables[@]}"; do
  found=0
  for table in "${tables[@]}"; do
    if [[ "$table" == "$required_table" ]]; then
      found=1
      break
    fi
  done
  [[ "$found" -eq 1 ]] || die "required table is missing: $required_table"
done

dump_args=()
{
  printf 'table\trows\n'
  for table in "${tables[@]}"; do
    [[ "$table" =~ ^(thread_db|task_db|eval_store|review_store|h[0-9a-f]{16})\.[a-z_]+$ ]] \
      || die "refusing unexpected table name: $table"
    IFS=. read -r schema name <<<"$table"
    count="$("${psql_cmd[@]}" -c "SELECT count(*) FROM \"$schema\".\"$name\"")"
    printf '%s\t%s\n' "$table" "$count"
    dump_args+=(--table="$table")
  done
} > "$OUT_DIR/table_counts.tsv"

"$PG_DUMP" \
  --format=custom \
  --no-owner \
  --no-acl \
  --lock-wait-timeout="$LOCK_WAIT_TIMEOUT" \
  "${dump_args[@]}" \
  --file="$OUT_DIR/phase1-data.dump" \
  "$HARNESS_DATABASE_URL"

"$PG_RESTORE" --list "$OUT_DIR/phase1-data.dump" > "$OUT_DIR/phase1-data.list"

cat > "$OUT_DIR/RESTORE.md" <<'EOF'
# Restore phase1 archive

This archive contains Harness Phase 1 contraction data captured before code
removal. It may include shared schemas (`thread_db`, `task_db`, `eval_store`,
`review_store`) and path-derived `h<hex>` schemas when they contain matching
thread, task, eval, or review tables.

Restore only into an empty scratch database:

```bash
export SCRATCH_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_phase1_restore
pg_restore --exit-on-error --no-owner --no-acl \
  --dbname "$SCRATCH_DATABASE_URL" phase1-data.dump
```

Do not restore directly into a live Harness database. Do not pass `--clean`.
Use `table_counts.tsv` and `phase1-data.list` to confirm expected objects first.
EOF

{
  echo
  echo "Created at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "Lock wait timeout: $LOCK_WAIT_TIMEOUT"
  echo "Table count: ${#tables[@]}"
} >> "$OUT_DIR/RESTORE.md"

printf 'Archive written: %s\n' "$OUT_DIR"
